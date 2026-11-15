use std::sync::Arc;

use async_trait::async_trait;

use super::error::DeployError;
use super::hetzner::HetznerProvider;
use super::lightsail::LightsailProvider;
use super::local::{self, LocalProvider};
use super::model::ProviderKind;
use super::provider::CloudProvider;
use super::service::{ProviderConfig, ProviderFactory};

pub struct EnvFactory;

#[async_trait]
impl ProviderFactory for EnvFactory {
    async fn build(&self, cfg: &ProviderConfig) -> Result<Arc<dyn CloudProvider>, DeployError> {
        match cfg.kind {
            ProviderKind::Hetzner => {
                let token = super::hetzner::load_token()?;
                let provider = if let Ok(endpoint) = std::env::var("HCLOUD_ENDPOINT") {
                    HetznerProvider::with_endpoint(endpoint, token)?
                } else {
                    HetznerProvider::new(token)?
                };
                Ok(Arc::new(provider))
            }
            ProviderKind::AwsLightsail => {
                let zone = if cfg.zone.is_empty() {
                    format!("{}a", cfg.region)
                } else {
                    cfg.zone.clone()
                };
                Ok(Arc::new(
                    LightsailProvider::connect(&cfg.region, &zone).await?,
                ))
            }
            ProviderKind::Local => Ok(Arc::new(LocalProvider::new(
                local::binary_from_env(),
                local::state_dir_from_env(),
            ))),
        }
    }
}
