use std::collections::BTreeMap;
use std::fmt;
use std::net::Ipv4Addr;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::{Platforms, QualityConfig, QualityMode};

use super::error::DeployError;

pub const TAG_MANAGED: &str = "echochamber-managed";
pub const TAG_ID: &str = "echochamber-deployment-id";
pub const TAG_EXPIRES: &str = "echochamber-expires-at";
pub const TAG_CREATED: &str = "echochamber-created-at";
pub const TAG_FIREWALL: &str = "echochamber-firewall-id";
pub const MANAGED_VALUE: &str = "true";

pub const RTMP_PORT: u16 = 1935;
pub const APP_NAME: &str = "echochamber";

pub const DEFAULT_TTL_SECS: u64 = 4 * 60 * 60;
pub const MAX_TTL_SECS: u64 = 24 * 60 * 60;
pub const MIN_TTL_SECS: u64 = 10 * 60;
pub const DEFAULT_IDLE_SECS: u64 = 10 * 60;
pub const DEFAULT_GRACE_SECS: u64 = 15 * 60;
pub const DEFAULT_READY_TIMEOUT: u64 = 10 * 60;
pub const MAX_READY_TIMEOUT: u64 = 30 * 60;

pub const DEFAULT_HETZNER_TYPE: &str = "cx23";
pub const DEFAULT_HETZNER_IMAGE: &str = "ubuntu-24.04";
pub const DEFAULT_LIGHTSAIL_BUNDLE: &str = "small_3_0";
pub const DEFAULT_LIGHTSAIL_BLUEPRINT: &str = "ubuntu_22_04";
pub const DEFAULT_EC2_TYPE: &str = "t3.small";
pub const DEFAULT_GCP_TYPE: &str = "e2-small";
pub const DEFAULT_GCP_IMAGE: &str =
    "projects/ubuntu-os-cloud/global/images/family/ubuntu-2404-lts-amd64";
pub const TAG_SECURITY_GROUP: &str = "echochamber-security-group";

/// Providers this build can provision. Fly.io and others implement the same trait later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    Hetzner,
    AwsLightsail,
    AwsEc2,
    Gcp,
    Local,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hetzner => "hetzner",
            Self::AwsLightsail => "aws-lightsail",
            Self::AwsEc2 => "aws-ec2",
            Self::Gcp => "gcp",
            Self::Local => "local",
        }
    }

    pub fn is_cloud(self) -> bool {
        match self {
            Self::Hetzner | Self::AwsLightsail | Self::AwsEc2 | Self::Gcp => true,
            Self::Local => false,
        }
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProviderKind {
    type Err = DeployError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "hetzner" => Ok(Self::Hetzner),
            "aws-lightsail" | "lightsail" => Ok(Self::AwsLightsail),
            "aws-ec2" | "ec2" => Ok(Self::AwsEc2),
            "gcp" | "google" => Ok(Self::Gcp),
            "local" => Ok(Self::Local),
            other => Err(DeployError::bad(format!(
                "unknown provider {other:?}; use hetzner, aws-lightsail, aws-ec2, gcp, or local"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeployStatus {
    Provisioning,
    Ready,
    Destroying,
    Destroyed,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmState {
    Pending,
    Running,
    Stopped,
    Terminated,
    Failed,
}

impl VmState {
    pub fn from_cloud(name: &str) -> Self {
        match name {
            "running" => Self::Running,
            "pending" | "initializing" | "starting" | "provisioning" | "staging" | "migrating"
            | "rebuilding" | "repairing" => Self::Pending,
            "stopping" | "stopped" | "shutting-down" | "off" | "suspended" | "suspending" => {
                Self::Stopped
            }
            "terminated" | "deleting" | "deleted" => Self::Terminated,
            _ => Self::Failed,
        }
    }

    /// Stopped and failed guests still cost money until the reaper deletes them.
    pub fn should_reap(self) -> bool {
        match self {
            Self::Stopped | Self::Terminated | Self::Failed => true,
            Self::Pending | Self::Running => false,
        }
    }
}

/// How the controller checks that the relay's control endpoint answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlPlane {
    /// The controller can HTTP this URL. Used for a relay on this machine.
    Reachable { url: String },
    /// Control listens on the guest's loopback. The guest holds the ingest
    /// port closed until its own request to `/status` succeeds.
    GuestLoopback,
}

#[derive(Debug, Clone)]
pub struct CloudInstance {
    pub deployment_id: String,
    pub provider_id: String,
    pub name: String,
    pub state: VmState,
    pub public_ip: Option<String>,
    pub rtmp_port: u16,
    pub control: ControlPlane,
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct InstancePlan {
    pub deployment_id: String,
    pub name: String,
    pub region: String,
    pub zone: String,
    pub image: String,
    pub prebuilt: bool,
    pub size: String,
    pub ingest_cidr: String,
    pub ingest_port: u16,
    pub user_data: String,
    pub tags: BTreeMap<String, String>,
    pub expires_at_unix: u64,
    pub created_at_unix: u64,
    pub stream_key: String,
    pub control_token: String,
    pub idle_secs: u64,
    pub connect_grace_secs: u64,
    pub ttl_secs: u64,
    pub platforms: Platforms,
    pub quality: QualityConfig,
    pub binary_url: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployRequest {
    pub provider: ProviderKind,
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub zone: Option<String>,
    #[serde(default = "default_ttl")]
    pub ttl_secs: u64,
    #[serde(default = "default_idle")]
    pub idle_secs: u64,
    #[serde(default = "default_grace")]
    pub connect_grace_secs: u64,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub prebuilt: bool,
    #[serde(default)]
    pub size: Option<String>,
    #[serde(default = "default_cidr")]
    pub ingest_cidr: String,
    #[serde(default)]
    pub binary_url: Option<String>,
    #[serde(default = "default_ready")]
    pub ready_timeout_secs: u64,
    /// Sent by the device from its keychain. Not stored.
    #[serde(default)]
    pub aws_access_key_id: Option<String>,
    #[serde(default)]
    pub aws_secret_access_key: Option<String>,
    #[serde(default)]
    pub aws_session_token: Option<String>,
    /// Service-account JSON from the device. Not stored.
    #[serde(default)]
    pub gcp_service_account_json: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
}

fn default_ttl() -> u64 {
    DEFAULT_TTL_SECS
}
fn default_idle() -> u64 {
    DEFAULT_IDLE_SECS
}
fn default_grace() -> u64 {
    DEFAULT_GRACE_SECS
}
fn default_cidr() -> String {
    "0.0.0.0/0".into()
}
fn default_ready() -> u64 {
    DEFAULT_READY_TIMEOUT
}

impl Default for DeployRequest {
    fn default() -> Self {
        Self {
            provider: ProviderKind::Local,
            region: String::new(),
            zone: None,
            ttl_secs: DEFAULT_TTL_SECS,
            idle_secs: DEFAULT_IDLE_SECS,
            connect_grace_secs: DEFAULT_GRACE_SECS,
            image: None,
            prebuilt: false,
            size: None,
            ingest_cidr: default_cidr(),
            binary_url: None,
            ready_timeout_secs: DEFAULT_READY_TIMEOUT,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            aws_session_token: None,
            gcp_service_account_json: None,
            project: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentRecord {
    pub id: String,
    pub provider: ProviderKind,
    pub region: String,
    pub zone: String,
    pub name: String,
    pub status: DeployStatus,
    pub public_ip: Option<String>,
    /// `rtmp://host:port/echochamber` — the OBS server field. Empty until ready.
    pub ingest_server: Option<String>,
    pub stream_key: Option<String>,
    pub expires_at_unix: u64,
    pub created_at_unix: u64,
    pub error: Option<String>,
}

impl DeploymentRecord {
    pub fn ingest_url(&self) -> Option<String> {
        match (&self.ingest_server, &self.stream_key) {
            (Some(server), Some(key)) if self.status == DeployStatus::Ready => {
                Some(format!("{server}/{key}"))
            }
            _ => None,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReapRequest {
    pub provider: ProviderKind,
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub aws_access_key_id: Option<String>,
    #[serde(default)]
    pub aws_secret_access_key: Option<String>,
    #[serde(default)]
    pub aws_session_token: Option<String>,
    #[serde(default)]
    pub gcp_service_account_json: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReapReport {
    pub dry_run: bool,
    pub provider: ProviderKind,
    pub deleted: Vec<String>,
    pub kept: Vec<String>,
    pub failed: Vec<String>,
}

pub fn managed_tags(id: &str, created: u64, expires: u64) -> BTreeMap<String, String> {
    BTreeMap::from([
        (TAG_MANAGED.to_string(), MANAGED_VALUE.to_string()),
        (TAG_ID.to_string(), id.to_string()),
        (TAG_CREATED.to_string(), created.to_string()),
        (TAG_EXPIRES.to_string(), expires.to_string()),
    ])
}

pub fn is_managed(tags: &BTreeMap<String, String>) -> bool {
    tags.get(TAG_MANAGED).map(String::as_str) == Some(MANAGED_VALUE)
}

pub fn expires_at(tags: &BTreeMap<String, String>) -> Option<u64> {
    tags.get(TAG_EXPIRES).and_then(|v| v.parse().ok())
}

/// Missing or past deadline, or a guest that is already stopped.
pub fn should_reap(inst: &CloudInstance, now: u64) -> bool {
    if !is_managed(&inst.tags) {
        return false;
    }
    if inst.state.should_reap() {
        return true;
    }
    match expires_at(&inst.tags) {
        Some(deadline) => deadline <= now,
        None => true,
    }
}

pub fn validate_request(req: &DeployRequest) -> Result<(), DeployError> {
    if req.ttl_secs < MIN_TTL_SECS || req.ttl_secs > MAX_TTL_SECS {
        return Err(DeployError::bad(format!(
            "ttl_secs must be between {MIN_TTL_SECS} and {MAX_TTL_SECS}"
        )));
    }
    if req.idle_secs < 30 || req.idle_secs >= req.ttl_secs {
        return Err(DeployError::bad(
            "idle_secs must be at least 30 and shorter than ttl_secs",
        ));
    }
    if req.connect_grace_secs < 60 || req.connect_grace_secs >= req.ttl_secs {
        return Err(DeployError::bad(
            "connect_grace_secs must be at least 60 and shorter than ttl_secs",
        ));
    }
    if req.ready_timeout_secs < 1 || req.ready_timeout_secs > MAX_READY_TIMEOUT {
        return Err(DeployError::bad(format!(
            "ready_timeout_secs must be between 1 and {MAX_READY_TIMEOUT}"
        )));
    }
    validate_cidr(&req.ingest_cidr)?;
    if let Some(url) = req.binary_url.as_deref() {
        validate_https_url(url)?;
    }
    if let Some(image) = req.image.as_deref() {
        validate_token_name("image", image)?;
    }
    if let Some(size) = req.size.as_deref() {
        validate_token_name("size", size)?;
    }
    if req.provider.is_cloud() {
        validate_region(&req.region)?;
        if let Some(zone) = req.zone.as_deref() {
            validate_region(zone)?;
        }
        if req.prebuilt && req.image.as_deref().unwrap_or("").is_empty() {
            return Err(DeployError::bad(
                "prebuilt deploys need --image (a snapshot name or id, or an AMI id)",
            ));
        }
    }
    Ok(())
}

pub fn validate_deployment_id(id: &str) -> Result<(), DeployError> {
    let ok = id.len() == 12
        && id.starts_with("ec")
        && id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() || b == b'e' || b == b'c');
    // `ec` plus 10 hex chars. The check above allows any hex; enforce the prefix length.
    let ok = ok && id[2..].chars().all(|c| c.is_ascii_hexdigit());
    if ok {
        Ok(())
    } else {
        Err(DeployError::bad(
            "deployment id must look like ec followed by 10 hex characters",
        ))
    }
}

pub fn validate_region(region: &str) -> Result<(), DeployError> {
    let ok = (2..=32).contains(&region.len())
        && region
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(DeployError::bad(format!(
            "region {region:?} must be 2-32 lowercase letters, digits, or hyphens"
        )))
    }
}

pub fn validate_cidr(cidr: &str) -> Result<(), DeployError> {
    let Some((ip, mask)) = cidr.split_once('/') else {
        return Err(DeployError::bad(format!(
            "ingest_cidr {cidr:?} must be an IPv4 CIDR such as 203.0.113.4/32"
        )));
    };
    if ip.parse::<Ipv4Addr>().is_err() {
        return Err(DeployError::bad(format!(
            "ingest_cidr {cidr:?} must be an IPv4 CIDR"
        )));
    }
    match mask.parse::<u8>() {
        Ok(n) if n <= 32 => Ok(()),
        _ => Err(DeployError::bad(format!(
            "ingest_cidr {cidr:?} mask must be 0-32"
        ))),
    }
}

pub fn validate_https_url(url: &str) -> Result<(), DeployError> {
    let ok = url.starts_with("https://")
        && url.len() <= 500
        && !url.contains('@')
        && url
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._~:/?#[]!$&'()*+,;=%".contains(c));
    if ok {
        Ok(())
    } else {
        Err(DeployError::bad(
            "binary_url must be an https URL with no userinfo",
        ))
    }
}

fn validate_token_name(field: &str, value: &str) -> Result<(), DeployError> {
    let ok = (1..=128).contains(&value.len())
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._:/-".contains(c));
    if ok {
        Ok(())
    } else {
        Err(DeployError::bad(format!(
            "{field} may only contain letters, digits, and . _ : / -"
        )))
    }
}

pub fn new_deployment_id() -> Result<String, DeployError> {
    Ok(format!("ec{}", random_hex(5)?))
}

pub fn new_secret(bytes: usize) -> Result<String, DeployError> {
    random_hex(bytes)
}

fn random_hex(bytes: usize) -> Result<String, DeployError> {
    let mut buf = vec![0u8; bytes];
    getrandom::getrandom(&mut buf).map_err(|e| DeployError::fail(format!("random: {e}")))?;
    let mut out = String::with_capacity(bytes * 2);
    for b in buf {
        out.push_str(&format!("{b:02x}"));
    }
    Ok(out)
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn default_zone(kind: ProviderKind, region: &str, zone: Option<&str>) -> String {
    if let Some(zone) = zone {
        if !zone.is_empty() {
            return zone.to_string();
        }
    }
    match kind {
        ProviderKind::AwsLightsail | ProviderKind::AwsEc2 => format!("{region}a"),
        ProviderKind::Gcp => format!("{region}-a"),
        ProviderKind::Hetzner | ProviderKind::Local => region.to_string(),
    }
}

pub fn default_image(
    kind: ProviderKind,
    image: Option<&str>,
    prebuilt: bool,
) -> Result<String, DeployError> {
    if let Some(image) = image {
        if !image.is_empty() {
            return Ok(image.to_string());
        }
    }
    match kind {
        ProviderKind::Hetzner if !prebuilt => Ok(DEFAULT_HETZNER_IMAGE.to_string()),
        ProviderKind::AwsLightsail if !prebuilt => Ok(DEFAULT_LIGHTSAIL_BLUEPRINT.to_string()),
        ProviderKind::Gcp if !prebuilt => Ok(DEFAULT_GCP_IMAGE.to_string()),
        ProviderKind::Local => Ok(String::new()),
        ProviderKind::AwsEc2 => Err(DeployError::bad(
            "EC2 needs an AMI id in --image, for example ami-0123456789abcdef0",
        )),
        ProviderKind::Hetzner | ProviderKind::AwsLightsail | ProviderKind::Gcp => Err(
            DeployError::bad("prebuilt deploys need --image (a snapshot name or id)"),
        ),
    }
}

pub fn default_size(kind: ProviderKind, size: Option<&str>) -> String {
    if let Some(size) = size {
        if !size.is_empty() {
            return size.to_string();
        }
    }
    match kind {
        ProviderKind::Hetzner => DEFAULT_HETZNER_TYPE.to_string(),
        ProviderKind::AwsLightsail => DEFAULT_LIGHTSAIL_BUNDLE.to_string(),
        ProviderKind::AwsEc2 => DEFAULT_EC2_TYPE.to_string(),
        ProviderKind::Gcp => DEFAULT_GCP_TYPE.to_string(),
        ProviderKind::Local => String::new(),
    }
}

pub fn toml_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

/// Config for one relay process. YouTube OAuth client secrets are not copied;
/// pushing only needs the RTMP stream key.
pub fn relay_config_toml(
    ingest_bind: &str,
    control_bind: &str,
    stream_key: &str,
    control_token: &str,
    platforms: &Platforms,
    quality: &QualityConfig,
) -> String {
    let mut out = String::new();
    out.push_str("[ingest]\n");
    out.push_str(&format!("bind = \"{}\"\n", toml_escape(ingest_bind)));
    out.push_str(&format!("app = \"{APP_NAME}\"\n"));
    out.push_str(&format!("stream_key = \"{}\"\n\n", toml_escape(stream_key)));
    out.push_str("[control]\n");
    out.push_str(&format!("bind = \"{}\"\n", toml_escape(control_bind)));
    out.push_str(&format!("token = \"{}\"\n\n", toml_escape(control_token)));
    if let Some(yt) = &platforms.youtube {
        out.push_str("[platforms.youtube]\n");
        out.push_str(&format!("enabled = {}\n", yt.enabled));
        out.push_str(&format!(
            "stream_key = \"{}\"\n",
            toml_escape(&yt.stream_key)
        ));
        if let Some(base) = &yt.ingest_base {
            out.push_str(&format!("ingest_base = \"{}\"\n", toml_escape(base)));
        }
        out.push('\n');
    }
    if let Some(x) = &platforms.x {
        out.push_str("[platforms.x]\n");
        out.push_str(&format!("enabled = {}\n", x.enabled));
        out.push_str(&format!(
            "stream_key = \"{}\"\n",
            toml_escape(&x.stream_key)
        ));
        if let Some(base) = &x.ingest_base {
            out.push_str(&format!("ingest_base = \"{}\"\n", toml_escape(base)));
        }
        out.push('\n');
    }
    if let Some(tw) = &platforms.twitch {
        out.push_str("[platforms.twitch]\n");
        out.push_str(&format!("enabled = {}\n", tw.enabled));
        out.push_str(&format!(
            "stream_key = \"{}\"\n",
            toml_escape(&tw.stream_key)
        ));
        if let Some(base) = &tw.ingest_base {
            out.push_str(&format!("ingest_base = \"{}\"\n", toml_escape(base)));
        }
        out.push('\n');
    }
    let mode = match quality.mode {
        QualityMode::Copy => "copy",
        QualityMode::Reencode => "reencode",
    };
    out.push_str("[quality]\n");
    out.push_str(&format!("mode = \"{mode}\"\n"));
    out.push_str(&format!("preset = \"{}\"\n", toml_escape(&quality.preset)));
    out.push_str(&format!("crf = {}\n\n", quality.crf));
    out.push_str("[standby]\nenabled = false\n\n[subtitles]\nenabled = false\n");
    out
}

pub fn ingest_server(ip: &str, port: u16) -> String {
    format!("rtmp://{ip}:{port}/{APP_NAME}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::YoutubePlatform;

    #[test]
    fn ttl_and_cidr_limits() {
        let mut req = DeployRequest {
            provider: ProviderKind::Hetzner,
            region: "fsn1".into(),
            ..DeployRequest::default()
        };
        validate_request(&req).unwrap();
        req.ttl_secs = 60;
        assert!(validate_request(&req).is_err());
        req.ttl_secs = DEFAULT_TTL_SECS;
        req.ingest_cidr = "0.0.0.0/0".into();
        validate_request(&req).unwrap();
        req.ingest_cidr = "nope".into();
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn ids_and_unknown_provider() {
        assert!(validate_deployment_id("ec0123abcd01").is_ok());
        assert!(validate_deployment_id("live").is_err());
        assert_eq!(ProviderKind::from_str("gcp").unwrap(), ProviderKind::Gcp);
        assert_eq!(ProviderKind::from_str("ec2").unwrap(), ProviderKind::AwsEc2);
        assert!(ProviderKind::from_str("fly").is_err());
        assert_eq!(
            ProviderKind::from_str("lightsail").unwrap(),
            ProviderKind::AwsLightsail
        );
    }

    #[test]
    fn relay_config_keeps_rtmp_keys_and_drops_oauth() {
        let mut platforms = Platforms::default();
        platforms.youtube = Some(YoutubePlatform {
            enabled: true,
            stream_key: "yt-secret-key".into(),
            ingest_base: None,
            client_id: Some("id".into()),
            client_secret: Some("super-secret-oauth".into()),
            refresh_token: Some("refresh".into()),
        });
        let toml = relay_config_toml(
            "0.0.0.0:1935",
            "127.0.0.1:8080",
            "abc123",
            "tok123",
            &platforms,
            &QualityConfig::default(),
        );
        assert!(toml.contains("stream_key = \"yt-secret-key\""));
        assert!(toml.contains("token = \"tok123\""));
        assert!(!toml.contains("super-secret-oauth"));
        assert!(!toml.contains("client_secret"));
        assert!(toml.contains("bind = \"127.0.0.1:8080\""));
    }

    #[test]
    fn reap_skips_live_tagged_and_drops_expired() {
        let mut tags = managed_tags("ec0123abcd01", 1, 500);
        let running = CloudInstance {
            deployment_id: "ec0123abcd01".into(),
            provider_id: "1".into(),
            name: "ec0123abcd01".into(),
            state: VmState::Running,
            public_ip: None,
            rtmp_port: 1935,
            control: ControlPlane::GuestLoopback,
            tags: tags.clone(),
        };
        assert!(!should_reap(&running, 100));
        assert!(should_reap(&running, 500));
        tags.insert(TAG_MANAGED.into(), "false".into());
        let mut foreign = running.clone();
        foreign.tags = tags;
        assert!(!should_reap(&foreign, 9_000));
    }
}
