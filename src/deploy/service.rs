use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

use async_trait::async_trait;

use super::cloud_init::{self, CloudInitSpec};
use super::credentials::{CredentialFields, RequestCredentials};
use super::error::DeployError;
use super::health::wait_until_ready;
use super::model::{
    default_image, default_size, default_zone, ingest_server, managed_tags, new_deployment_id,
    new_secret, now_unix, should_reap, validate_deployment_id, validate_request, DeployRequest,
    DeployStatus, DeploymentRecord, InstancePlan, ProviderKind, ReapReport, ReapRequest, RTMP_PORT,
};
use super::provider::CloudProvider;
use super::store::{self, Store};
use crate::config::{Platforms, QualityConfig};

pub trait Clock: Send + Sync {
    fn now_unix(&self) -> u64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix(&self) -> u64 {
        now_unix()
    }
}

pub struct FixedClock(pub u64);

impl Clock for FixedClock {
    fn now_unix(&self) -> u64 {
        self.0
    }
}

#[derive(Clone)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    pub region: String,
    pub zone: String,
    pub project: Option<String>,
    pub credentials: RequestCredentials,
}

#[async_trait]
pub trait ProviderFactory: Send + Sync {
    async fn build(&self, cfg: &ProviderConfig) -> Result<Arc<dyn CloudProvider>, DeployError>;
}

#[derive(Clone)]
pub struct RelaySeed {
    pub platforms: Platforms,
    pub quality: QualityConfig,
}

impl Default for RelaySeed {
    fn default() -> Self {
        Self {
            platforms: Platforms::default(),
            quality: QualityConfig::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SweepTarget {
    pub provider: ProviderKind,
    pub region: String,
    pub project: Option<String>,
}

#[derive(Debug, Default)]
pub struct SweepReport {
    pub deleted: Vec<String>,
    pub kept: Vec<String>,
    pub failed: Vec<String>,
}

pub struct DeployService {
    factory: Arc<dyn ProviderFactory>,
    store: Store,
    clock: Arc<dyn Clock>,
    poll: Duration,
}

impl DeployService {
    pub fn new(
        factory: Arc<dyn ProviderFactory>,
        store: Store,
        clock: Arc<dyn Clock>,
        poll: Duration,
    ) -> Self {
        Self {
            factory,
            store,
            clock,
            poll,
        }
    }

    pub fn from_env() -> Self {
        Self::new(
            Arc::new(super::factory::EnvFactory),
            Store::new(store::store_path_from_env()),
            Arc::new(SystemClock),
            Duration::from_secs(5),
        )
    }

    pub fn list(&self) -> Result<Vec<DeploymentRecord>, DeployError> {
        self.store.list()
    }

    pub fn get(&self, id: &str) -> Result<DeploymentRecord, DeployError> {
        validate_deployment_id(id)?;
        self.store
            .get(id)?
            .ok_or_else(|| DeployError::missing(format!("no deployment {id}")))
    }

    pub async fn deploy(
        &self,
        req: DeployRequest,
        seed: RelaySeed,
    ) -> Result<DeploymentRecord, DeployError> {
        validate_request(&req)?;
        let id = new_deployment_id()?;
        let stream_key = new_secret(16)?;
        let control_token = new_secret(32)?;
        let created = self.clock.now_unix();
        let expires = created + req.ttl_secs;
        let zone = default_zone(req.provider, req.region.trim(), req.zone.as_deref());
        let image = default_image(req.provider, req.image.as_deref(), req.prebuilt)?;
        let size = default_size(req.provider, req.size.as_deref());
        let tags = managed_tags(&id, created, expires);
        let credentials = req.credentials()?;
        let project = req.project.clone().or_else(|| {
            credentials
                .gcp
                .as_ref()
                .map(|account| account.project_id.clone())
        });
        let user_data = if req.provider.is_cloud() {
            cloud_init::render(&CloudInitSpec {
                stream_key: &stream_key,
                control_token: &control_token,
                platforms: &seed.platforms,
                quality: &seed.quality,
                expires_at_unix: expires,
                idle_secs: req.idle_secs,
                connect_grace_secs: req.connect_grace_secs,
                prebuilt: req.prebuilt,
                binary_url: req.binary_url.as_deref(),
            })?
        } else {
            String::new()
        };
        let plan = InstancePlan {
            deployment_id: id.clone(),
            name: id.clone(),
            region: if req.provider == ProviderKind::Local {
                "local".into()
            } else {
                req.region.trim().to_string()
            },
            zone: zone.clone(),
            image,
            prebuilt: req.prebuilt,
            size,
            ingest_cidr: req.ingest_cidr.clone(),
            ingest_port: RTMP_PORT,
            user_data,
            tags: tags.clone(),
            expires_at_unix: expires,
            created_at_unix: created,
            stream_key: stream_key.clone(),
            control_token: control_token.clone(),
            idle_secs: req.idle_secs,
            connect_grace_secs: req.connect_grace_secs,
            ttl_secs: req.ttl_secs,
            platforms: seed.platforms,
            quality: seed.quality,
            binary_url: req.binary_url.clone(),
        };
        let cfg = ProviderConfig {
            kind: req.provider,
            region: plan.region.clone(),
            zone: zone.clone(),
            project: project.clone(),
            credentials,
        };
        let mut record = DeploymentRecord {
            id: id.clone(),
            provider: req.provider,
            region: plan.region.clone(),
            zone,
            name: id.clone(),
            status: DeployStatus::Provisioning,
            public_ip: None,
            ingest_server: None,
            stream_key: None,
            project: project.clone(),
            expires_at_unix: expires,
            created_at_unix: created,
            error: None,
        };
        self.save(&record)?;
        let provider = self.factory.build(&cfg).await?;
        if let Err(err) = provider.create(&plan).await {
            let _ = provider.destroy(&id).await;
            record.status = DeployStatus::Error;
            record.error = Some(scrub(
                &err.to_string(),
                &plan.stream_key,
                &plan.control_token,
            ));
            self.save(&record)?;
            return Err(err);
        }
        let ready = match wait_until_ready(
            provider.as_ref(),
            &id,
            &control_token,
            self.poll,
            Duration::from_secs(req.ready_timeout_secs),
        )
        .await
        {
            Ok(inst) => inst,
            Err(err) => {
                let _ = provider.destroy(&id).await;
                record.status = DeployStatus::Error;
                record.error = Some(scrub(
                    &err.to_string(),
                    &plan.stream_key,
                    &plan.control_token,
                ));
                self.save(&record)?;
                return Err(err);
            }
        };
        if let Err(err) = provider.restrict_ingress(&id).await {
            let _ = provider.destroy(&id).await;
            record.status = DeployStatus::Error;
            record.error = Some(scrub(
                &err.to_string(),
                &plan.stream_key,
                &plan.control_token,
            ));
            self.save(&record)?;
            return Err(err);
        }
        let ip = ready
            .public_ip
            .clone()
            .filter(|ip| !ip.is_empty())
            .ok_or_else(|| DeployError::fail("ready relay has no address"))?;
        record.status = DeployStatus::Ready;
        record.public_ip = Some(ip.clone());
        record.ingest_server = Some(ingest_server(&ip, ready.rtmp_port));
        record.stream_key = Some(stream_key);
        record.error = None;
        self.save(&record)?;
        tracing::info!(
            id = %record.id,
            provider = %record.provider,
            region = %record.region,
            "relay ready"
        );
        Ok(record)
    }

    pub async fn teardown(
        &self,
        id: &str,
        fields: CredentialFields,
    ) -> Result<DeploymentRecord, DeployError> {
        validate_deployment_id(id)?;
        let mut record = self
            .store
            .get(id)?
            .ok_or_else(|| DeployError::missing(format!("no deployment {id}")))?;
        let creds = fields.credentials_for(record.provider)?;
        record.status = DeployStatus::Destroying;
        self.save(&record)?;
        let provider = self
            .factory
            .build(&ProviderConfig {
                kind: record.provider,
                region: record.region.clone(),
                zone: record.zone.clone(),
                project: fields.project.clone(),
                credentials: creds,
            })
            .await?;
        provider.destroy(id).await?;
        record.status = DeployStatus::Destroyed;
        record.error = None;
        self.save(&record)?;
        Ok(record)
    }

    pub async fn reap(&self, req: ReapRequest) -> Result<ReapReport, DeployError> {
        if req.provider.is_cloud() && req.region.trim().is_empty() {
            return Err(DeployError::bad("reap needs a region"));
        }
        let credentials = req.credentials()?;
        let zone = default_zone(req.provider, req.region.trim(), None);
        let provider = self
            .factory
            .build(&ProviderConfig {
                kind: req.provider,
                region: if req.provider == ProviderKind::Local {
                    "local".into()
                } else {
                    req.region.trim().to_string()
                },
                zone,
                project: req.project.clone(),
                credentials,
            })
            .await?;
        let now = self.clock.now_unix();
        let found = provider.list_managed().await?;
        let mut report = ReapReport {
            dry_run: req.dry_run,
            provider: req.provider,
            deleted: Vec::new(),
            kept: Vec::new(),
            failed: Vec::new(),
        };
        for inst in found {
            if !should_reap(&inst, now) {
                report.kept.push(inst.deployment_id);
                continue;
            }
            if req.dry_run {
                report.deleted.push(inst.deployment_id);
                continue;
            }
            match provider.destroy(&inst.deployment_id).await {
                Ok(()) => {
                    let _ = self.store.mark_destroyed(&inst.deployment_id);
                    report.deleted.push(inst.deployment_id);
                }
                Err(err) => report.failed.push(format!("{}: {err}", inst.deployment_id)),
            }
        }
        Ok(report)
    }

    /// One pass: delete this machine's relays that are expired or already
    /// stopped, then sweep each provider and region for labeled leftovers.
    pub async fn sweep_once(&self, extras: &[SweepTarget]) -> SweepReport {
        let mut report = SweepReport::default();
        let records = match self.store.list() {
            Ok(rows) => rows,
            Err(err) => {
                report.failed.push(err.to_string());
                return report;
            }
        };
        let now = self.clock.now_unix();
        for record in records
            .iter()
            .filter(|row| row.status != DeployStatus::Destroyed)
        {
            let expired = record.expires_at_unix > 0 && record.expires_at_unix <= now;
            let cfg = config_for(record, None);
            let provider = match self.factory.build(&cfg).await {
                Ok(provider) => provider,
                Err(err) => {
                    report.failed.push(format!("{}: {err}", record.id));
                    continue;
                }
            };
            let guest_stopped = match provider.describe(&record.id).await {
                Ok(inst) => inst.state.should_reap(),
                Err(_) => false,
            };
            if expired || guest_stopped {
                match provider.destroy(&record.id).await {
                    Ok(()) => {
                        let _ = self.store.mark_destroyed(&record.id);
                        report.deleted.push(record.id.clone());
                    }
                    Err(err) => report.failed.push(format!("{}: {err}", record.id)),
                }
            } else {
                report.kept.push(record.id.clone());
            }
        }
        let mut targets = targets_from(&records);
        for extra in extras {
            if !targets.iter().any(|target| same_target(target, extra)) {
                targets.push(extra.clone());
            }
        }
        for target in targets {
            let req = ReapRequest {
                provider: target.provider,
                region: target.region.clone(),
                dry_run: false,
                aws_access_key_id: None,
                aws_secret_access_key: None,
                aws_session_token: None,
                gcp_service_account_json: None,
                project: target.project.clone(),
            };
            match self.reap(req).await {
                Ok(swept) => {
                    report.deleted.extend(swept.deleted);
                    report.kept.extend(swept.kept);
                    report.failed.extend(swept.failed);
                }
                Err(err) => report
                    .failed
                    .push(format!("{} {}: {err}", target.provider, target.region)),
            }
        }
        report
    }

    /// Repeats `sweep_once` until `shutdown` is signaled. A provider error
    /// is recorded and the loop continues.
    pub async fn run_reaper(
        self: &Arc<Self>,
        interval: Duration,
        extras: Vec<SweepTarget>,
        shutdown: Arc<Notify>,
    ) {
        loop {
            let report = self.sweep_once(&extras).await;
            if !report.failed.is_empty() {
                tracing::warn!(
                    errors = report.failed.len(),
                    "deploy sweep had provider errors"
                );
            } else if !report.deleted.is_empty() {
                tracing::info!(
                    deleted = report.deleted.len(),
                    "deploy sweep removed relays"
                );
            }
            tokio::select! {
                _ = shutdown.notified() => break,
                _ = tokio::time::sleep(interval) => {}
            }
        }
    }

    /// The deployment file keeps the ingest server, not the stream key.
    fn save(&self, record: &DeploymentRecord) -> Result<(), DeployError> {
        let mut stored = record.clone();
        stored.stream_key = None;
        self.store.upsert(&stored)
    }
}

fn scrub(text: &str, stream_key: &str, control_token: &str) -> String {
    let mut out = text.to_string();
    for secret in [stream_key, control_token] {
        if secret.len() >= 6 {
            out = out.replace(secret, "***");
        }
    }
    if out.len() > 400 {
        out.truncate(400);
    }
    out
}

fn config_for(record: &DeploymentRecord, project: Option<String>) -> ProviderConfig {
    ProviderConfig {
        kind: record.provider,
        region: record.region.clone(),
        zone: if record.zone.is_empty() {
            default_zone(record.provider, &record.region, None)
        } else {
            record.zone.clone()
        },
        project: project.or_else(|| record.project.clone()),
        credentials: RequestCredentials::default(),
    }
}

fn targets_from(records: &[DeploymentRecord]) -> Vec<SweepTarget> {
    let mut targets = Vec::new();
    for record in records {
        let target = SweepTarget {
            provider: record.provider,
            region: record.region.clone(),
            project: record.project.clone(),
        };
        if !targets
            .iter()
            .any(|existing| same_target(existing, &target))
        {
            targets.push(target);
        }
    }
    targets
}

fn same_target(left: &SweepTarget, right: &SweepTarget) -> bool {
    left.provider == right.provider && left.region == right.region && left.project == right.project
}

#[cfg(test)]
mod sweep_tests {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use tokio::sync::Notify;

    use super::*;
    use crate::deploy::error::DeployError;
    use crate::deploy::mock::MockProvider;
    use crate::deploy::model::{CloudInstance, ControlPlane, DeployStatus, VmState};
    use crate::deploy::provider::CloudProvider;
    use crate::deploy::store::Store;

    fn record(id: &str, expires: u64) -> DeploymentRecord {
        DeploymentRecord {
            id: id.into(),
            provider: ProviderKind::Hetzner,
            region: "fsn1".into(),
            zone: "fsn1".into(),
            name: id.into(),
            status: DeployStatus::Ready,
            public_ip: Some("203.0.113.9".into()),
            ingest_server: Some("rtmp://203.0.113.9:1935/echochamber".into()),
            stream_key: None,
            project: None,
            expires_at_unix: expires,
            created_at_unix: 1,
            error: None,
        }
    }

    fn guest(id: &str, state: VmState, expires: u64, managed: bool) -> CloudInstance {
        let mut tags = crate::deploy::model::managed_tags(id, 1, expires);
        if !managed {
            tags.insert("echochamber-managed".into(), "no".into());
        }
        CloudInstance {
            deployment_id: id.into(),
            provider_id: id.into(),
            name: id.into(),
            state,
            public_ip: Some("203.0.113.9".into()),
            rtmp_port: 1935,
            control: ControlPlane::GuestLoopback,
            tags,
        }
    }

    struct Flaky {
        fail: AtomicBool,
        builds: AtomicU32,
        provider: Arc<MockProvider>,
    }

    #[async_trait]
    impl ProviderFactory for Flaky {
        async fn build(
            &self,
            _cfg: &ProviderConfig,
        ) -> Result<Arc<dyn CloudProvider>, DeployError> {
            self.builds.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Err(DeployError::fail("provider down"));
            }
            Ok(self.provider.clone())
        }
    }

    fn service(provider: Arc<MockProvider>, clock: u64) -> DeployService {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deployments.json");
        std::mem::forget(dir);
        DeployService::new(
            Arc::new(crate::deploy::mock::FixedFactory { provider }),
            Store::new(path),
            Arc::new(FixedClock(clock)),
            Duration::from_millis(20),
        )
    }

    #[tokio::test]
    async fn sweep_deletes_expired_and_stopped_only() {
        let mock = Arc::new(MockProvider::new(ProviderKind::Hetzner));
        mock.insert(guest("ec1111111111", VmState::Running, 100, true));
        mock.insert(guest("ec2222222222", VmState::Running, 9_000, true));
        mock.insert(guest("ec3333333333", VmState::Stopped, 9_000, true));
        mock.insert(guest("ec4444444444", VmState::Running, 1, false));
        let svc = service(Arc::clone(&mock), 1_000);
        svc.store.upsert(&record("ec1111111111", 100)).unwrap();
        let mut live = record("ec2222222222", 9_000);
        live.stream_key = Some("plain-stream-key".into());
        svc.save(&live).unwrap();
        svc.store.upsert(&record("ec3333333333", 9_000)).unwrap();
        let report = svc.sweep_once(&[]).await;
        let destroyed: Vec<_> = mock
            .events()
            .into_iter()
            .filter_map(|event| match event {
                crate::deploy::mock::MockEvent::Destroy(id) => Some(id),
                _ => None,
            })
            .collect();
        assert!(
            destroyed.iter().any(|id| id == "ec1111111111"),
            "{report:?}"
        );
        assert!(
            destroyed.iter().any(|id| id == "ec3333333333"),
            "{report:?}"
        );
        assert!(
            !destroyed.iter().any(|id| id == "ec2222222222"),
            "{report:?}"
        );
        assert!(
            !destroyed.iter().any(|id| id == "ec4444444444"),
            "{report:?}"
        );
        let stored = std::fs::read_to_string(svc.store.path()).unwrap();
        assert!(!stored.contains("plain-stream-key"));
    }

    #[tokio::test]
    async fn a_provider_error_does_not_stop_the_loop() {
        let mock = Arc::new(MockProvider::new(ProviderKind::Gcp));
        mock.insert(guest("ec1111111111", VmState::Running, 100, true));
        let fail = AtomicBool::new(true);
        let builds = AtomicU32::new(0);
        let factory = Arc::new(Flaky {
            fail,
            builds,
            provider: Arc::clone(&mock),
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deployments.json");
        std::mem::forget(dir);
        let svc = Arc::new(DeployService::new(
            factory.clone(),
            Store::new(path),
            Arc::new(FixedClock(1_000)),
            Duration::from_millis(10),
        ));
        svc.store.upsert(&record("ec1111111111", 100)).unwrap();
        let stop = Arc::new(Notify::new());
        let running = Arc::clone(&svc);
        let shutdown = Arc::clone(&stop);
        let task = tokio::spawn(async move {
            running
                .run_reaper(Duration::from_millis(15), Vec::new(), shutdown)
                .await;
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            if factory.builds.load(Ordering::SeqCst) >= 2 {
                factory.fail.store(false, Ordering::SeqCst);
            }
            if mock.events().iter().any(|event| {
                matches!(event, crate::deploy::mock::MockEvent::Destroy(id) if id == "ec1111111111")
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        stop.notify_waiters();
        let _ = tokio::time::timeout(Duration::from_secs(1), task).await;
        assert!(
            factory.builds.load(Ordering::SeqCst) >= 3,
            "sweep stopped after the first provider error"
        );
        assert!(mock.events().iter().any(|event| {
            matches!(event, crate::deploy::mock::MockEvent::Destroy(id) if id == "ec1111111111")
        }));
    }
}
