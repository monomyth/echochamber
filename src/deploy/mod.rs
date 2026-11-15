//! Ephemeral relays. Hetzner, Lightsail, EC2, Compute Engine, Fly.io, and this computer.
//! The streaming path is unchanged.

mod awsutil;
mod cloud_init;
mod credentials;
mod ec2;
mod error;
mod factory;
mod fly;
mod gcp;
mod health;
mod hetzner;
mod lightsail;
mod local;
mod mock;
mod model;
mod provider;
mod service;
mod store;

pub use credentials::{CredentialFields, RequestCredentials};
pub use error::DeployError;
pub use mock::{FixedFactory, MockEndpoint, MockEvent, MockProvider};
pub use model::{
    CloudInstance, ControlPlane, DeployRequest, DeployStatus, DeploymentRecord, ProviderKind,
    ReapReport, ReapRequest, VmState, DEFAULT_GRACE_SECS, DEFAULT_IDLE_SECS, DEFAULT_READY_TIMEOUT,
    DEFAULT_TTL_SECS,
};
pub use service::{Clock, DeployService, FixedClock, RelaySeed, SweepTarget, SystemClock};
pub use store::Store;

pub fn cloud_init_process_script(
    stream_key: &str,
    control_token: &str,
    expires_at_unix: u64,
    idle_secs: u64,
    connect_grace_secs: u64,
    prebuilt: bool,
    binary_url: Option<&str>,
    platforms: &crate::config::Platforms,
    quality: &crate::config::QualityConfig,
) -> Result<String, DeployError> {
    cloud_init::render_process(&cloud_init::CloudInitSpec {
        stream_key,
        control_token,
        platforms,
        quality,
        expires_at_unix,
        idle_secs,
        connect_grace_secs,
        prebuilt,
        binary_url,
    })
}

pub fn cloud_init_script(
    stream_key: &str,
    control_token: &str,
    expires_at_unix: u64,
    idle_secs: u64,
    connect_grace_secs: u64,
    prebuilt: bool,
    binary_url: Option<&str>,
    platforms: &crate::config::Platforms,
    quality: &crate::config::QualityConfig,
) -> Result<String, DeployError> {
    cloud_init::render(&cloud_init::CloudInitSpec {
        stream_key,
        control_token,
        platforms,
        quality,
        expires_at_unix,
        idle_secs,
        connect_grace_secs,
        prebuilt,
        binary_url,
    })
}
