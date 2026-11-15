use std::sync::Arc;
use std::time::Duration;

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
            project: req.project.clone(),
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
            expires_at_unix: expires,
            created_at_unix: created,
            error: None,
        };
        self.store.upsert(&record)?;
        let provider = self.factory.build(&cfg).await?;
        if let Err(err) = provider.create(&plan).await {
            let _ = provider.destroy(&id).await;
            record.status = DeployStatus::Error;
            record.error = Some(err.to_string());
            self.store.upsert(&record)?;
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
                record.error = Some(err.to_string());
                self.store.upsert(&record)?;
                return Err(err);
            }
        };
        if let Err(err) = provider.restrict_ingress(&id).await {
            let _ = provider.destroy(&id).await;
            record.status = DeployStatus::Error;
            record.error = Some(err.to_string());
            self.store.upsert(&record)?;
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
        self.store.upsert(&record)?;
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
        self.store.upsert(&record)?;
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
        self.store.upsert(&record)?;
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
}
