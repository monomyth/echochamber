use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::error::DeployError;
use super::model::{
    is_managed, validate_deployment_id, CloudInstance, ControlPlane, InstancePlan, ProviderKind,
    VmState, MANAGED_VALUE, RTMP_PORT, TAG_EXPIRES, TAG_ID, TAG_MANAGED,
};
use super::provider::CloudProvider;

pub const DEFAULT_BASE: &str = "https://api.machines.dev";
const APP_PREFIX: &str = "echochamber-";
const INGEST_CIDR_META: &str = "echochamber-ingest-cidr";

pub struct FlyProvider {
    http: reqwest::Client,
    base: String,
    token: String,
    org: String,
    region: String,
}

impl FlyProvider {
    pub fn connect(region: &str) -> Result<Self, DeployError> {
        let base = std::env::var("FLY_API_HOSTNAME").unwrap_or_else(|_| DEFAULT_BASE.to_string());
        Self::with_endpoint(base, load_token()?, load_org()?, region)
    }

    pub fn with_endpoint(
        base: impl Into<String>,
        token: impl Into<String>,
        org: impl Into<String>,
        region: &str,
    ) -> Result<Self, DeployError> {
        let org = org.into();
        path_segment("org", &org)?;
        if !region.is_empty() {
            path_segment("region", region)?;
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| DeployError::fail(format!("fly client: {e}")))?;
        Ok(Self {
            http,
            base: base.into().trim_end_matches('/').to_string(),
            token: token.into(),
            org,
            region: region.to_string(),
        })
    }
}

pub fn load_token() -> Result<String, DeployError> {
    for key in ["FLY_API_TOKEN", "FLY_ACCESS_TOKEN"] {
        if let Ok(value) = std::env::var(key) {
            let value = value.trim();
            if !value.is_empty() {
                return Ok(value.trim_start_matches("Bearer ").to_string());
            }
        }
    }
    let path = std::env::var("FLY_CONFIG")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            directories::BaseDirs::new()
                .map(|d| d.home_dir().join(".fly/config.yml"))
                .unwrap_or_else(|| std::path::PathBuf::from(".fly/config.yml"))
        });
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Some(token) = token_from_fly_config(&text) {
            return Ok(token);
        }
    }
    Err(DeployError::fail(
        "no Fly token on this machine. Set FLY_API_TOKEN. Create it with `fly tokens create org` in an org that contains only relay apps. The token is not a request field.",
    ))
}

pub fn load_org() -> Result<String, DeployError> {
    for key in ["FLY_ORG", "FLY_ORGANIZATION"] {
        if let Ok(value) = std::env::var(key) {
            let value = value.trim();
            if !value.is_empty() {
                return Ok(value.to_string());
            }
        }
    }
    Err(DeployError::bad(
        "Fly.io needs FLY_ORG, the slug of an org that contains only these relays.",
    ))
}

pub fn token_from_fly_config(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("access_token:") else {
            continue;
        };
        let token = rest.trim().trim_matches('"').trim_matches('\'').trim();
        if !token.is_empty() {
            return Some(token.to_string());
        }
    }
    None
}

pub fn app_name(deployment_id: &str) -> Result<String, DeployError> {
    validate_deployment_id(deployment_id)?;
    Ok(format!("{APP_PREFIX}{deployment_id}"))
}

pub fn guest_from_size(size: &str) -> Result<Value, DeployError> {
    let (preset, memory) = match size.split_once(':') {
        Some((preset, memory)) => {
            let memory = memory.parse::<u32>().map_err(|_| {
                DeployError::bad("Fly size memory must be a number of megabytes after ':'")
            })?;
            (preset, Some(memory))
        }
        None => (size, None),
    };
    let (cpu_kind, cpus, default_memory) = match preset {
        "shared-cpu-1x" => ("shared", 1, 1024),
        "shared-cpu-2x" => ("shared", 2, 2048),
        "shared-cpu-4x" => ("shared", 4, 4096),
        "shared-cpu-8x" => ("shared", 8, 8192),
        "performance-1x" => ("performance", 1, 2048),
        "performance-2x" => ("performance", 2, 4096),
        "performance-4x" => ("performance", 4, 8192),
        "performance-8x" => ("performance", 8, 16384),
        other => {
            return Err(DeployError::bad(format!(
                "unknown Fly size {other:?}; use shared-cpu-1x or performance-1x, optionally :memory_mb"
            )));
        }
    };
    let memory_mb = memory.unwrap_or(default_memory);
    if !(256..=16384).contains(&memory_mb) {
        return Err(DeployError::bad(
            "Fly memory must be between 256 and 16384 MB",
        ));
    }
    Ok(json!({
        "cpu_kind": cpu_kind,
        "cpus": cpus,
        "memory_mb": memory_mb,
    }))
}

pub fn machine_body(plan: &InstancePlan) -> Result<Value, DeployError> {
    if plan.user_data.trim().is_empty() {
        return Err(DeployError::fail("fly machine is missing its boot script"));
    }
    if plan.ingest_port != RTMP_PORT {
        return Err(DeployError::bad("fly ingest port is 1935"));
    }
    let mut metadata = plan.tags.clone();
    metadata.insert(INGEST_CIDR_META.to_string(), plan.ingest_cidr.clone());
    Ok(json!({
        "name": plan.deployment_id,
        "region": plan.region,
        "config": {
            "image": plan.image,
            "auto_destroy": true,
            "restart": { "policy": "no" },
            "guest": guest_from_size(&plan.size)?,
            "metadata": metadata,
            "services": [{
                "protocol": "tcp",
                "internal_port": RTMP_PORT,
                "autostart": false,
                "autostop": "off",
                "ports": [{ "port": RTMP_PORT }]
            }],
            "init": { "exec": ["/bin/bash", "-c", plan.user_data] }
        }
    }))
}

#[async_trait]
impl CloudProvider for FlyProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Fly
    }

    async fn create(&self, plan: &InstancePlan) -> Result<CloudInstance, DeployError> {
        let app = app_name(&plan.deployment_id)?;
        let body = machine_body(plan)?;
        if let Err(err) = self
            .post(
                "/v1/apps",
                &json!({ "app_name": app, "org_slug": self.org }),
            )
            .await
        {
            return Err(err);
        }
        let allocated = match self
            .post(
                &format!("/v1/apps/{app}/ip_assignments"),
                &json!({ "type": "v4" }),
            )
            .await
        {
            Ok(value) => value,
            Err(err) => {
                let _ = self.delete_ok(&format!("/v1/apps/{app}")).await;
                return Err(err);
            }
        };
        let address = ip_address_of(&allocated).ok_or_else(|| {
            DeployError::fail("fly allocated an address but the response had none")
        })?;
        let created = match self.post(&format!("/v1/apps/{app}/machines"), &body).await {
            Ok(value) => value,
            Err(err) => {
                let _ = self.delete_ok(&format!("/v1/apps/{app}")).await;
                return Err(err);
            }
        };
        let mut inst = parse_machine(&created)?;
        inst.public_ip = Some(address);
        Ok(inst)
    }

    async fn describe(&self, deployment_id: &str) -> Result<CloudInstance, DeployError> {
        let app = app_name(deployment_id)?;
        let machines = match self.machines(&app, None).await {
            Ok(machines) => machines,
            Err(err) if err.to_string().contains("http 404") => {
                return Err(DeployError::missing(format!("no fly app {deployment_id}")));
            }
            Err(err) => return Err(err),
        };
        let ip = self.public_v4(&app).await?;
        if let Some(machine) = machines.iter().find(|machine| {
            metadata_of(machine).get(TAG_ID).map(String::as_str) == Some(deployment_id)
        }) {
            let mut inst = parse_machine(machine)?;
            if inst.public_ip.is_none() {
                inst.public_ip = ip;
            }
            return Ok(inst);
        }
        Ok(CloudInstance {
            deployment_id: deployment_id.to_string(),
            provider_id: app.clone(),
            name: app,
            state: VmState::Terminated,
            public_ip: ip,
            rtmp_port: RTMP_PORT,
            control: ControlPlane::GuestLoopback,
            tags: BTreeMap::from([
                (TAG_MANAGED.to_string(), MANAGED_VALUE.to_string()),
                (TAG_ID.to_string(), deployment_id.to_string()),
            ]),
        })
    }

    async fn destroy(&self, deployment_id: &str) -> Result<(), DeployError> {
        let app = app_name(deployment_id)?;
        match self.machines(&app, None).await {
            Ok(machines) => {
                for machine in machines {
                    let Some(id) = machine["id"].as_str() else {
                        continue;
                    };
                    path_segment("machine", id)?;
                    let _ = self
                        .post_empty(&format!("/v1/apps/{app}/machines/{id}/stop"))
                        .await;
                    self.delete_ok(&format!("/v1/apps/{app}/machines/{id}?force=true"))
                        .await?;
                }
            }
            Err(err) if err.to_string().contains("http 404") => {}
            Err(err) => return Err(err),
        }
        self.delete_ok(&format!("/v1/apps/{app}")).await
    }

    async fn list_managed(&self) -> Result<Vec<CloudInstance>, DeployError> {
        let mut found = Vec::new();
        for app in self.list_apps().await? {
            let Some(name) = app["name"].as_str() else {
                continue;
            };
            let Some(deployment_id) = name.strip_prefix(APP_PREFIX) else {
                continue;
            };
            if validate_deployment_id(deployment_id).is_err() {
                continue;
            }
            let machines = self.machines(name, None).await?;
            let in_region: Vec<&Value> = machines
                .iter()
                .filter(|machine| {
                    self.region.is_empty()
                        || machine["region"].as_str() == Some(self.region.as_str())
                })
                .filter(|machine| is_managed(&metadata_of(machine)))
                .collect();
            if in_region.is_empty() {
                if machines.is_empty() {
                    found.push(CloudInstance {
                        deployment_id: deployment_id.to_string(),
                        provider_id: name.to_string(),
                        name: name.to_string(),
                        state: VmState::Terminated,
                        public_ip: None,
                        rtmp_port: RTMP_PORT,
                        control: ControlPlane::GuestLoopback,
                        tags: BTreeMap::from([
                            (TAG_MANAGED.to_string(), MANAGED_VALUE.to_string()),
                            (TAG_ID.to_string(), deployment_id.to_string()),
                            (TAG_EXPIRES.to_string(), "0".to_string()),
                        ]),
                    });
                }
                continue;
            }
            let ip = self.public_v4(name).await?;
            for machine in in_region {
                let mut inst = parse_machine(machine)?;
                if inst.public_ip.is_none() {
                    inst.public_ip = ip.clone();
                }
                found.push(inst);
            }
        }
        Ok(found)
    }

    async fn restrict_ingress(&self, deployment_id: &str) -> Result<(), DeployError> {
        let app = app_name(deployment_id)?;
        let machines = self.machines(&app, None).await?;
        let Some(machine) = machines.iter().find(|machine| {
            metadata_of(machine).get(TAG_ID).map(String::as_str) == Some(deployment_id)
        }) else {
            return Ok(());
        };
        if services_are_ingest_only(&machine["config"]["services"]) {
            return Ok(());
        }
        let id = machine["id"]
            .as_str()
            .ok_or_else(|| DeployError::fail("fly machine has no id"))?;
        path_segment("machine", id)?;
        let mut config = machine["config"].clone();
        config["services"] = json!([{
            "protocol": "tcp",
            "internal_port": RTMP_PORT,
            "autostart": false,
            "autostop": "off",
            "ports": [{ "port": RTMP_PORT }]
        }]);
        self.post(
            &format!("/v1/apps/{app}/machines/{id}"),
            &json!({ "config": config }),
        )
        .await?;
        Ok(())
    }
}

impl FlyProvider {
    async fn list_apps(&self) -> Result<Vec<Value>, DeployError> {
        let mut cursor = String::new();
        let mut all = Vec::new();
        for _ in 0..20 {
            let path = if cursor.is_empty() {
                format!("/v1/apps?org_slug={}", self.org)
            } else {
                format!("/v1/apps?org_slug={}&cursor={cursor}", self.org)
            };
            let body = self.get(&path).await?;
            if let Some(apps) = body.as_array() {
                all.extend(apps.iter().cloned());
                break;
            }
            if let Some(apps) = body["apps"].as_array() {
                all.extend(apps.iter().cloned());
            }
            match body["next"].as_str().or_else(|| body["cursor"].as_str()) {
                Some(next) if !next.is_empty() && next != cursor => cursor = next.to_string(),
                _ => break,
            }
        }
        Ok(all)
    }

    async fn machines(&self, app: &str, region: Option<&str>) -> Result<Vec<Value>, DeployError> {
        path_segment("app", app)?;
        let mut path = format!("/v1/apps/{app}/machines?metadata.{TAG_MANAGED}={MANAGED_VALUE}");
        if let Some(region) = region {
            if !region.is_empty() {
                path.push_str("&region=");
                path.push_str(region);
            }
        }
        let body = self.get(&path).await?;
        if let Some(items) = body.as_array() {
            return Ok(items.clone());
        }
        Ok(body["machines"].as_array().cloned().unwrap_or_default())
    }

    async fn public_v4(&self, app: &str) -> Result<Option<String>, DeployError> {
        let body = match self.get(&format!("/v1/apps/{app}/ip_assignments")).await {
            Ok(body) => body,
            Err(err) if err.to_string().contains("http 404") => return Ok(None),
            Err(err) => return Err(err),
        };
        Ok(first_v4(&body))
    }

    async fn get(&self, path: &str) -> Result<Value, DeployError> {
        self.send(self.request(reqwest::Method::GET, path)).await
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value, DeployError> {
        self.send(self.request(reqwest::Method::POST, path).json(body))
            .await
    }

    async fn post_empty(&self, path: &str) -> Result<Value, DeployError> {
        self.send(self.request(reqwest::Method::POST, path)).await
    }

    async fn delete_ok(&self, path: &str) -> Result<(), DeployError> {
        let response = self
            .request(reqwest::Method::DELETE, path)
            .send()
            .await
            .map_err(|e| DeployError::fail(format!("fly: {e}")))?;
        if response.status().as_u16() == 404 || response.status().is_success() {
            return Ok(());
        }
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        Err(DeployError::fail(fly_error(status.as_u16(), &text)))
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {}", self.token))
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Value, DeployError> {
        let response = request
            .send()
            .await
            .map_err(|e| DeployError::fail(format!("fly: {e}")))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(DeployError::fail(fly_error(status.as_u16(), &text)));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|_| DeployError::fail("fly response was not json"))
    }
}

fn fly_error(status: u16, body: &str) -> String {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value.get("error").and_then(|error| {
                error.as_str().map(str::to_string).or_else(|| {
                    error
                        .get("message")
                        .and_then(|m| m.as_str())
                        .map(str::to_string)
                })
            })
        })
        .filter(|text| {
            text.len() < 180
                && !text.contains("PRIVATE")
                && !text.contains("stream_key")
                && !text.contains("fm2_")
        })
        .unwrap_or_else(|| "request failed".to_string());
    format!("fly http {status}: {message}")
}

fn path_segment(field: &str, value: &str) -> Result<(), DeployError> {
    let ok = !value.is_empty()
        && value.len() <= 63
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if ok && !value.contains("..") {
        Ok(())
    } else {
        Err(DeployError::bad(format!(
            "fly {field} must be a short lowercase id"
        )))
    }
}

fn metadata_of(machine: &Value) -> BTreeMap<String, String> {
    let mut tags = BTreeMap::new();
    let Some(obj) = machine["config"]["metadata"].as_object() else {
        return tags;
    };
    for (key, value) in obj {
        if let Some(text) = value.as_str() {
            tags.insert(key.clone(), text.to_string());
        }
    }
    tags
}

fn parse_machine(machine: &Value) -> Result<CloudInstance, DeployError> {
    let tags = metadata_of(machine);
    let deployment_id = tags
        .get(TAG_ID)
        .cloned()
        .ok_or_else(|| DeployError::fail("fly machine is missing its deployment id"))?;
    validate_deployment_id(&deployment_id)?;
    let id = machine["id"].as_str().unwrap_or("").to_string();
    Ok(CloudInstance {
        deployment_id,
        provider_id: id,
        name: machine["name"].as_str().unwrap_or("").to_string(),
        state: fly_state(machine["state"].as_str().unwrap_or("")),
        public_ip: None,
        rtmp_port: RTMP_PORT,
        control: ControlPlane::GuestLoopback,
        tags,
    })
}

fn fly_state(name: &str) -> VmState {
    match name {
        "started" => VmState::Running,
        "created" | "starting" | "replacing" => VmState::Pending,
        "stopping" | "stopped" | "suspended" => VmState::Stopped,
        "destroying" | "destroyed" => VmState::Terminated,
        other => VmState::from_cloud(other),
    }
}

fn ip_address_of(value: &Value) -> Option<String> {
    value["address"]
        .as_str()
        .or_else(|| value["ip_address"]["address"].as_str())
        .or_else(|| value["ip"]["address"].as_str())
        .map(str::to_string)
        .filter(|ip| !ip.is_empty())
}

fn first_v4(value: &Value) -> Option<String> {
    let lists = [
        value["addresses"].as_array(),
        value["ips"].as_array(),
        value["ip_assignments"].as_array(),
        value.as_array(),
    ];
    for list in lists.into_iter().flatten() {
        for item in list {
            let kind = item["type"].as_str().unwrap_or("");
            if kind == "v4" || kind == "dedicated_v4" {
                if let Some(address) = item["address"].as_str() {
                    if !address.is_empty() {
                        return Some(address.to_string());
                    }
                }
            }
        }
    }
    None
}

fn services_are_ingest_only(services: &Value) -> bool {
    let Some(services) = services.as_array() else {
        return false;
    };
    if services.len() != 1 {
        return false;
    }
    let service = &services[0];
    if service["protocol"].as_str() != Some("tcp") {
        return false;
    }
    if service["internal_port"].as_u64() != Some(u64::from(RTMP_PORT)) {
        return false;
    }
    let Some(ports) = service["ports"].as_array() else {
        return false;
    };
    ports.len() == 1 && ports[0]["port"].as_u64() == Some(u64::from(RTMP_PORT))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::QualityConfig;
    use axum::extract::{Path, Query, State};
    use axum::routing::{delete, post};
    use axum::{Json, Router};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    fn plan() -> InstancePlan {
        let mut tags = super::super::model::managed_tags("ec0123abcd01", 10, 9_000);
        tags.insert(TAG_EXPIRES.into(), "9000".into());
        InstancePlan {
            deployment_id: "ec0123abcd01".into(),
            name: "ec0123abcd01".into(),
            region: "iad".into(),
            zone: "iad".into(),
            image: "registry-1.docker.io/library/ubuntu:24.04".into(),
            prebuilt: true,
            size: "shared-cpu-1x".into(),
            ingest_cidr: "203.0.113.8/32".into(),
            ingest_port: 1935,
            user_data: "#!/bin/bash\necho hi\n".into(),
            tags,
            expires_at_unix: 9_000,
            created_at_unix: 10,
            stream_key: "abc".into(),
            control_token: "def".into(),
            idle_secs: 60,
            connect_grace_secs: 120,
            ttl_secs: 3600,
            platforms: Default::default(),
            quality: QualityConfig::default(),
            binary_url: None,
        }
    }

    #[test]
    fn launch_spec_is_tcp_1935_and_does_not_restart() {
        let body = machine_body(&plan()).unwrap();
        assert_eq!(body["config"]["auto_destroy"], true);
        assert_eq!(body["config"]["restart"]["policy"], "no");
        assert_eq!(body["config"]["guest"]["cpu_kind"], "shared");
        assert_eq!(body["config"]["guest"]["memory_mb"], 1024);
        assert_eq!(body["config"]["metadata"][TAG_MANAGED], "true");
        assert_eq!(
            body["config"]["metadata"][INGEST_CIDR_META],
            "203.0.113.8/32"
        );
        let services = body["config"]["services"].as_array().unwrap();
        assert_eq!(services.len(), 1);
        assert_eq!(services[0]["internal_port"], 1935);
        assert_eq!(services[0]["ports"][0]["port"], 1935);
        assert!(services[0]["ports"][0].get("handlers").is_none());
        assert!(!body.to_string().contains("\"port\":22"));
        assert!(!body.to_string().contains("\"port\":80"));
    }

    #[test]
    fn config_file_token_is_read_without_printing_neighbors() {
        let text = "access_token: \"FlyV1 fm2_example\"\norg: personal\n";
        assert_eq!(
            token_from_fly_config(text).as_deref(),
            Some("FlyV1 fm2_example")
        );
        assert!(token_from_fly_config("wireguard: true\n").is_none());
    }

    #[derive(Default)]
    struct Fake {
        apps: Vec<App>,
        auth: Vec<String>,
        actions: Vec<String>,
        created: Option<Value>,
    }

    struct App {
        name: String,
        machines: Vec<Value>,
        address: Option<String>,
    }

    #[tokio::test]
    async fn lifecycle_stays_on_localhost() {
        let fake = Arc::new(Mutex::new(Fake::default()));
        let app = Router::new()
            .route("/v1/apps", post(create_app).get(list_apps))
            .route("/v1/apps/{name}", delete(delete_app))
            .route(
                "/v1/apps/{name}/ip_assignments",
                post(assign_ip).get(list_ips),
            )
            .route(
                "/v1/apps/{name}/machines",
                post(create_machine).get(list_machines),
            )
            .route("/v1/apps/{name}/machines/{id}/stop", post(stop_machine))
            .route("/v1/apps/{name}/machines/{id}", delete(delete_machine))
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let provider =
            FlyProvider::with_endpoint(format!("http://{addr}"), "test-token", "relay-org", "iad")
                .unwrap();
        let created = provider.create(&plan()).await.unwrap();
        assert_eq!(created.public_ip.as_deref(), Some("203.0.113.40"));
        assert!(matches!(created.control, ControlPlane::GuestLoopback));
        assert_eq!(created.state, VmState::Running);
        let described = provider.describe("ec0123abcd01").await.unwrap();
        assert_eq!(described.public_ip.as_deref(), Some("203.0.113.40"));
        provider.restrict_ingress("ec0123abcd01").await.unwrap();

        {
            let mut guard = fake.lock().unwrap();
            guard.apps.push(App {
                name: "blog".into(),
                machines: vec![json!({
                    "id": "other",
                    "name": "blog",
                    "state": "started",
                    "region": "iad",
                    "config": { "metadata": {} }
                })],
                address: None,
            });
            guard.apps.push(App {
                name: "echochamber-ec2222222222".into(),
                machines: vec![managed_machine("ec2222222222", "fra", "started")],
                address: Some("203.0.113.41".into()),
            });
            guard.apps.push(App {
                name: "echochamber-ec3333333333".into(),
                machines: vec![managed_machine("ec3333333333", "iad", "stopped")],
                address: Some("203.0.113.42".into()),
            });
            guard.apps.push(App {
                name: "echochamber-ec4444444444".into(),
                machines: Vec::new(),
                address: None,
            });
        }
        let listed = provider.list_managed().await.unwrap();
        let ids: Vec<_> = listed
            .iter()
            .map(|item| item.deployment_id.as_str())
            .collect();
        assert!(ids.contains(&"ec0123abcd01"));
        assert!(ids.contains(&"ec3333333333"));
        assert!(ids.contains(&"ec4444444444"));
        assert!(!ids.contains(&"ec2222222222"));
        assert!(listed
            .iter()
            .any(|item| item.deployment_id == "ec3333333333" && item.state == VmState::Stopped));
        assert!(listed
            .iter()
            .any(|item| item.deployment_id == "ec4444444444" && item.state == VmState::Terminated));

        provider.destroy("ec0123abcd01").await.unwrap();
        let actions = fake.lock().unwrap().actions.clone();
        let stop = actions.iter().position(|item| item == "stop").unwrap();
        let deleted = actions
            .iter()
            .position(|item| item == "delete-machine")
            .unwrap();
        let app_gone = actions
            .iter()
            .position(|item| item == "delete-app")
            .unwrap();
        assert!(stop < deleted && deleted < app_gone);
        assert!(fake
            .lock()
            .unwrap()
            .apps
            .iter()
            .all(|app| app.name != "echochamber-ec0123abcd01"));
        let auth = fake.lock().unwrap().auth.clone();
        assert!(auth.iter().all(|header| header == "Bearer test-token"));
        let spec = fake.lock().unwrap().created.clone().unwrap();
        assert_eq!(spec["config"]["restart"]["policy"], "no");
        assert_eq!(spec["config"]["auto_destroy"], true);
        assert!(!spec.to_string().contains("test-token"));
    }

    fn managed_machine(id: &str, region: &str, state: &str) -> Value {
        json!({
            "id": format!("m-{id}"),
            "name": id,
            "state": state,
            "region": region,
            "config": {
                "metadata": {
                    "echochamber-managed": "true",
                    "echochamber-deployment-id": id,
                    "echochamber-expires-at": "9000"
                },
                "services": [{
                    "protocol": "tcp",
                    "internal_port": 1935,
                    "ports": [{ "port": 1935 }]
                }]
            }
        })
    }

    async fn create_app(
        State(fake): State<Arc<Mutex<Fake>>>,
        headers: axum::http::HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        note(&fake, &headers);
        assert_eq!(body["org_slug"], "relay-org");
        let name = body["app_name"].as_str().unwrap().to_string();
        fake.lock().unwrap().apps.push(App {
            name,
            machines: Vec::new(),
            address: None,
        });
        Json(json!({"id": "app"}))
    }

    async fn list_apps(
        State(fake): State<Arc<Mutex<Fake>>>,
        headers: axum::http::HeaderMap,
    ) -> Json<Value> {
        note(&fake, &headers);
        let apps: Vec<_> = fake
            .lock()
            .unwrap()
            .apps
            .iter()
            .map(|app| json!({"name": app.name}))
            .collect();
        Json(json!({ "apps": apps }))
    }

    async fn assign_ip(
        State(fake): State<Arc<Mutex<Fake>>>,
        headers: axum::http::HeaderMap,
        Path(name): Path<String>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        note(&fake, &headers);
        assert_eq!(body["type"], "v4");
        let mut guard = fake.lock().unwrap();
        let app = guard.apps.iter_mut().find(|app| app.name == name).unwrap();
        app.address = Some("203.0.113.40".into());
        Json(json!({"address": "203.0.113.40", "type": "v4"}))
    }

    async fn list_ips(
        State(fake): State<Arc<Mutex<Fake>>>,
        headers: axum::http::HeaderMap,
        Path(name): Path<String>,
    ) -> Json<Value> {
        note(&fake, &headers);
        let address = fake
            .lock()
            .unwrap()
            .apps
            .iter()
            .find(|app| app.name == name)
            .and_then(|app| app.address.clone());
        let addresses = match address {
            Some(address) => vec![json!({"address": address, "type": "v4"})],
            None => Vec::new(),
        };
        Json(json!({ "addresses": addresses }))
    }

    async fn create_machine(
        State(fake): State<Arc<Mutex<Fake>>>,
        headers: axum::http::HeaderMap,
        Path(name): Path<String>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        note(&fake, &headers);
        assert_eq!(body["config"]["services"][0]["ports"][0]["port"], 1935);
        assert_eq!(body["config"]["restart"]["policy"], "no");
        assert_eq!(body["config"]["auto_destroy"], true);
        let mut stored = body.clone();
        stored["id"] = json!("machine-1");
        stored["state"] = json!("started");
        fake.lock().unwrap().created = Some(body);
        fake.lock()
            .unwrap()
            .apps
            .iter_mut()
            .find(|app| app.name == name)
            .unwrap()
            .machines
            .push(stored.clone());
        Json(stored)
    }

    async fn list_machines(
        State(fake): State<Arc<Mutex<Fake>>>,
        headers: axum::http::HeaderMap,
        Path(name): Path<String>,
        Query(query): Query<HashMap<String, String>>,
    ) -> Json<Value> {
        note(&fake, &headers);
        assert!(query.keys().any(|key| key.starts_with("metadata.")));
        let machines = fake
            .lock()
            .unwrap()
            .apps
            .iter()
            .find(|app| app.name == name)
            .map(|app| app.machines.clone())
            .unwrap_or_default();
        Json(Value::Array(machines))
    }

    async fn stop_machine(
        State(fake): State<Arc<Mutex<Fake>>>,
        headers: axum::http::HeaderMap,
    ) -> Json<Value> {
        note(&fake, &headers);
        fake.lock().unwrap().actions.push("stop".into());
        Json(json!({"ok": true}))
    }

    async fn delete_machine(
        State(fake): State<Arc<Mutex<Fake>>>,
        headers: axum::http::HeaderMap,
        Path((name, id)): Path<(String, String)>,
    ) -> StatusNoContent {
        note(&fake, &headers);
        fake.lock().unwrap().actions.push("delete-machine".into());
        if let Some(app) = fake
            .lock()
            .unwrap()
            .apps
            .iter_mut()
            .find(|app| app.name == name)
        {
            app.machines
                .retain(|machine| machine["id"].as_str() != Some(id.as_str()));
        }
        StatusNoContent
    }

    async fn delete_app(
        State(fake): State<Arc<Mutex<Fake>>>,
        headers: axum::http::HeaderMap,
        Path(name): Path<String>,
    ) -> StatusNoContent {
        note(&fake, &headers);
        fake.lock().unwrap().actions.push("delete-app".into());
        fake.lock().unwrap().apps.retain(|app| app.name != name);
        StatusNoContent
    }

    fn note(fake: &Arc<Mutex<Fake>>, headers: &axum::http::HeaderMap) {
        let header = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        fake.lock().unwrap().auth.push(header);
    }

    struct StatusNoContent;

    impl axum::response::IntoResponse for StatusNoContent {
        fn into_response(self) -> axum::response::Response {
            axum::http::StatusCode::NO_CONTENT.into_response()
        }
    }
}
