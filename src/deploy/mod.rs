//! Ephemeral relays. Hetzner is the default cloud, Lightsail is the second,
//! and `local` runs the relay on this machine. The streaming path is unchanged.

mod cloud_init;
mod error;
mod factory;
mod health;
mod hetzner;
mod lightsail;
mod local;
mod mock;
mod model;
mod provider;
mod service;
mod store;

pub use error::DeployError;
pub use mock::{FixedFactory, MockEndpoint, MockEvent, MockProvider};
pub use model::{
    CloudInstance, ControlPlane, DeployRequest, DeployStatus, DeploymentRecord, ProviderKind,
    ReapReport, ReapRequest, VmState, DEFAULT_GRACE_SECS, DEFAULT_IDLE_SECS, DEFAULT_READY_TIMEOUT,
    DEFAULT_TTL_SECS,
};
pub use service::{Clock, DeployService, FixedClock, RelaySeed, SystemClock};
pub use store::Store;

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
