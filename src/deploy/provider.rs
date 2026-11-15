use async_trait::async_trait;

use super::error::DeployError;
use super::model::{CloudInstance, InstancePlan, ProviderKind};

/// One cloud, or this computer. Adding a provider is a new `ProviderKind`
/// and a new implementation; readiness, tagging, and the reaper stay here.
#[async_trait]
pub trait CloudProvider: Send + Sync {
    fn kind(&self) -> ProviderKind;

    async fn create(&self, plan: &InstancePlan) -> Result<CloudInstance, DeployError>;

    async fn describe(&self, deployment_id: &str) -> Result<CloudInstance, DeployError>;

    async fn destroy(&self, deployment_id: &str) -> Result<(), DeployError>;

    /// Resources this provider created for echochamber. Untagged resources must not appear.
    async fn list_managed(&self) -> Result<Vec<CloudInstance>, DeployError>;

    /// Make sure the only public ingress is the ingest port. Local is a no-op.
    async fn restrict_ingress(&self, deployment_id: &str) -> Result<(), DeployError> {
        let _ = deployment_id;
        Ok(())
    }
}
