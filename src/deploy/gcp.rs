use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::credentials::RequestCredentials;
use super::error::DeployError;
use super::model::{
    is_managed, validate_deployment_id, CloudInstance, ControlPlane, InstancePlan, ProviderKind,
    VmState, MANAGED_VALUE, RTMP_PORT, TAG_ID, TAG_MANAGED,
};
use super::provider::CloudProvider;

pub const COMPUTE_BASE: &str = "https://compute.googleapis.com/compute/v1";
const COMPUTE_SCOPE: &str = "https://www.googleapis.com/auth/compute";

#[async_trait]
pub trait GcpTokens: Send + Sync {
    async fn bearer(&self) -> Result<String, DeployError>;
}

pub struct AuthTokens {
    inner: Arc<dyn gcp_auth::TokenProvider>,
}

#[async_trait]
impl GcpTokens for AuthTokens {
    async fn bearer(&self) -> Result<String, DeployError> {
        let token = self
            .inner
            .token(&[COMPUTE_SCOPE])
            .await
            .map_err(|e| DeployError::fail(format!("gcp auth: {e}")))?;
        Ok(token.as_str().to_string())
    }
}

pub struct GcpProvider {
    http: reqwest::Client,
    base: String,
    project: String,
    tokens: Arc<dyn GcpTokens>,
}

impl GcpProvider {
    pub async fn connect(
        project: Option<&str>,
        creds: &RequestCredentials,
    ) -> Result<Self, DeployError> {
        let (tokens, account_project): (Arc<dyn GcpTokens>, Option<String>) =
            if let Some(account) = &creds.gcp {
                let parsed = gcp_auth::CustomServiceAccount::from_json(account.json())
                    .map_err(|_| DeployError::fail("gcp service account key could not be used"))?;
                let project = parsed.project_id().map(str::to_string);
                (
                    Arc::new(AuthTokens {
                        inner: Arc::new(parsed),
                    }),
                    project,
                )
            } else {
                let provider = gcp_auth::provider().await.map_err(|e| {
                    DeployError::fail(format!("gcp application default credentials: {e}"))
                })?;
                let project = provider.project_id().await.ok().map(|p| p.to_string());
                (Arc::new(AuthTokens { inner: provider }), project)
            };
        let project = project
            .map(str::to_string)
            .or(account_project)
            .ok_or_else(|| {
                DeployError::bad(
                    "GCP needs --project or a service account JSON that includes project_id",
                )
            })?;
        path_segment("project", &project)?;
        let base =
            std::env::var("GCP_COMPUTE_ENDPOINT").unwrap_or_else(|_| COMPUTE_BASE.to_string());
        Self::with_tokens(base, project, tokens)
    }

    pub fn with_tokens(
        base: impl Into<String>,
        project: impl Into<String>,
        tokens: Arc<dyn GcpTokens>,
    ) -> Result<Self, DeployError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| DeployError::fail(format!("gcp client: {e}")))?;
        Ok(Self {
            http,
            base: base.into().trim_end_matches('/').to_string(),
            project: project.into(),
            tokens,
        })
    }
}

pub fn instance_body(plan: &InstancePlan) -> Value {
    let tag = format!("echochamber-{}", plan.deployment_id);
    json!({
        "name": plan.name,
        "machineType": format!("zones/{}/machineTypes/{}", plan.zone, plan.size),
        "labels": plan.tags,
        "tags": { "items": ["echochamber", tag] },
        "disks": [{
            "boot": true,
            "autoDelete": true,
            "initializeParams": {
                "sourceImage": plan.image,
                "diskSizeGb": "20"
            }
        }],
        "networkInterfaces": [{
            "network": "global/networks/default",
            "accessConfigs": [{ "name": "external-nat", "type": "ONE_TO_ONE_NAT" }]
        }],
        "serviceAccounts": [{ "email": "default", "scopes": [] }],
        "metadata": {
            "items": [
                { "key": "startup-script", "value": plan.user_data },
                { "key": "block-project-ssh-keys", "value": "true" }
            ]
        },
        "scheduling": {
            "automaticRestart": false,
            "onHostMaintenance": "TERMINATE",
            "instanceTerminationAction": "DELETE",
            "maxRunDuration": { "seconds": plan.ttl_secs.to_string() }
        }
    })
}

pub fn ingest_firewall(plan: &InstancePlan) -> Value {
    json!({
        "name": format!("{}-in", plan.name),
        "network": "global/networks/default",
        "direction": "INGRESS",
        "priority": 1000,
        "targetTags": [format!("echochamber-{}", plan.deployment_id)],
        "allowed": [{ "IPProtocol": "tcp", "ports": [plan.ingest_port.to_string()] }],
        "sourceRanges": [plan.ingest_cidr],
        "labels": plan.tags
    })
}

pub fn deny_admin_firewall(plan: &InstancePlan) -> Value {
    json!({
        "name": format!("{}-deny", plan.name),
        "network": "global/networks/default",
        "direction": "INGRESS",
        "priority": 900,
        "targetTags": [format!("echochamber-{}", plan.deployment_id)],
        "denied": [{ "IPProtocol": "tcp", "ports": ["22", "3389"] }],
        "sourceRanges": ["0.0.0.0/0"],
        "labels": plan.tags
    })
}

#[async_trait]
impl CloudProvider for GcpProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Gcp
    }

    async fn create(&self, plan: &InstancePlan) -> Result<CloudInstance, DeployError> {
        path_segment("zone", &plan.zone)?;
        path_segment("name", &plan.name)?;
        self.insert_firewall(&ingest_firewall(plan)).await?;
        if let Err(err) = self.insert_firewall(&deny_admin_firewall(plan)).await {
            let _ = self.delete_firewall(&format!("{}-in", plan.name)).await;
            return Err(err);
        }
        let op = match self
            .post(
                &format!("/projects/{}/zones/{}/instances", self.project, plan.zone),
                &instance_body(plan),
            )
            .await
        {
            Ok(op) => op,
            Err(err) => {
                let _ = self.delete_firewall(&format!("{}-in", plan.name)).await;
                let _ = self.delete_firewall(&format!("{}-deny", plan.name)).await;
                return Err(err);
            }
        };
        if let Err(err) = self.wait_op(&op).await {
            let _ = self.delete_firewall(&format!("{}-in", plan.name)).await;
            let _ = self.delete_firewall(&format!("{}-deny", plan.name)).await;
            return Err(err);
        }
        let body = self
            .get(&format!(
                "/projects/{}/zones/{}/instances/{}",
                self.project, plan.zone, plan.name
            ))
            .await?;
        parse_instance(&body)
    }

    async fn describe(&self, deployment_id: &str) -> Result<CloudInstance, DeployError> {
        validate_deployment_id(deployment_id)?;
        self.find(deployment_id)
            .await?
            .ok_or_else(|| DeployError::missing(format!("no gcp instance {deployment_id}")))
    }

    async fn destroy(&self, deployment_id: &str) -> Result<(), DeployError> {
        validate_deployment_id(deployment_id)?;
        if let Some(inst) = self.find(deployment_id).await? {
            if let Some(zone) = inst.tags.get("echochamber-zone") {
                let _ = self
                    .delete_ok(&format!(
                        "/projects/{}/zones/{zone}/instances/{}",
                        self.project, inst.name
                    ))
                    .await;
            }
        }
        let _ = self.delete_firewall(&format!("{deployment_id}-in")).await;
        let _ = self.delete_firewall(&format!("{deployment_id}-deny")).await;
        Ok(())
    }

    async fn list_managed(&self) -> Result<Vec<CloudInstance>, DeployError> {
        let mut found = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let filter = urlencoding("labels.echochamber-managed eq true");
        let body = self
            .get(&format!(
                "/projects/{}/aggregated/instances?filter={filter}",
                self.project
            ))
            .await?;
        if let Some(items) = body["items"].as_object() {
            for scoped in items.values() {
                if let Some(instances) = scoped["instances"].as_array() {
                    for instance in instances {
                        if let Ok(parsed) = parse_instance(instance) {
                            if is_managed(&parsed.tags) {
                                seen.insert(parsed.deployment_id.clone());
                                found.push(parsed);
                            }
                        }
                    }
                }
            }
        }
        let firewalls = self
            .get(&format!(
                "/projects/{}/global/firewalls?filter={filter}",
                self.project
            ))
            .await?;
        if let Some(items) = firewalls["items"].as_array() {
            for firewall in items {
                let labels = labels_of(firewall);
                let Some(id) = labels.get(TAG_ID).cloned() else {
                    continue;
                };
                if seen.contains(&id) || !is_managed(&labels) {
                    continue;
                }
                found.push(CloudInstance {
                    deployment_id: id,
                    provider_id: firewall["id"].as_str().unwrap_or("").to_string(),
                    name: firewall["name"].as_str().unwrap_or("").to_string(),
                    state: VmState::Terminated,
                    public_ip: None,
                    rtmp_port: RTMP_PORT,
                    control: ControlPlane::GuestLoopback,
                    tags: labels,
                });
            }
        }
        Ok(found)
    }
}

impl GcpProvider {
    async fn insert_firewall(&self, body: &Value) -> Result<(), DeployError> {
        let op = self
            .post(
                &format!("/projects/{}/global/firewalls", self.project),
                body,
            )
            .await?;
        self.wait_op(&op).await
    }

    async fn delete_firewall(&self, name: &str) -> Result<(), DeployError> {
        path_segment("firewall", name)?;
        self.delete_ok(&format!(
            "/projects/{}/global/firewalls/{name}",
            self.project
        ))
        .await
    }

    async fn find(&self, deployment_id: &str) -> Result<Option<CloudInstance>, DeployError> {
        let filter = urlencoding(&format!("name eq {deployment_id}"));
        let body = self
            .get(&format!(
                "/projects/{}/aggregated/instances?filter={filter}",
                self.project
            ))
            .await?;
        let mut found = Vec::new();
        if let Some(items) = body["items"].as_object() {
            for scoped in items.values() {
                if let Some(instances) = scoped["instances"].as_array() {
                    for instance in instances {
                        if let Ok(parsed) = parse_instance(instance) {
                            if parsed.deployment_id == deployment_id || parsed.name == deployment_id
                            {
                                found.push(parsed);
                            }
                        }
                    }
                }
            }
        }
        if found.len() > 1 {
            return Err(DeployError::fail(format!(
                "more than one gcp instance is named {deployment_id}"
            )));
        }
        Ok(found.pop())
    }

    async fn wait_op(&self, op: &Value) -> Result<(), DeployError> {
        let mut current = op.clone();
        for _ in 0..90 {
            if current["status"] == "DONE" {
                if let Some(message) = current["error"]["errors"][0]["message"].as_str() {
                    return Err(DeployError::fail(format!("gcp operation: {message}")));
                }
                if current.get("error").is_some() && !current["error"].is_null() {
                    return Err(DeployError::fail("gcp operation failed"));
                }
                return Ok(());
            }
            let name = current["name"].as_str().unwrap_or("");
            path_segment("operation", name)?;
            let path = match zone_name(current.get("zone").and_then(|v| v.as_str())) {
                Some(zone) => format!("/projects/{}/zones/{zone}/operations/{name}", self.project),
                None => format!("/projects/{}/global/operations/{name}", self.project),
            };
            tokio::time::sleep(Duration::from_secs(1)).await;
            current = self.get(&path).await?;
        }
        Err(DeployError::fail("gcp operation timed out"))
    }

    async fn get(&self, path: &str) -> Result<Value, DeployError> {
        self.send(self.request(reqwest::Method::GET, path)).await
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value, DeployError> {
        self.send(self.request(reqwest::Method::POST, path).json(body))
            .await
    }

    async fn delete_ok(&self, path: &str) -> Result<(), DeployError> {
        let token = self.tokens.bearer().await?;
        let response = self
            .request(reqwest::Method::DELETE, path)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| DeployError::fail(format!("gcp: {e}")))?;
        if response.status().as_u16() == 404 || response.status().is_success() {
            return Ok(());
        }
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        Err(DeployError::fail(gcp_error(status.as_u16(), &text)))
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http.request(method, format!("{}{path}", self.base))
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Value, DeployError> {
        let token = self.tokens.bearer().await?;
        let response = request
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| DeployError::fail(format!("gcp: {e}")))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(DeployError::fail(gcp_error(status.as_u16(), &text)));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|_| DeployError::fail("gcp response was not JSON"))
    }
}

fn gcp_error(status: u16, body: &str) -> String {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v["error"]["message"]
                .as_str()
                .or_else(|| v["error"]["errors"][0]["message"].as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "request failed".into());
    format!("gcp http {status}: {message}")
}

fn path_segment(label: &str, value: &str) -> Result<(), DeployError> {
    let ok = (1..=63).contains(&value.len())
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(DeployError::bad(format!(
            "gcp {label} {value:?} must be lowercase letters, digits, or hyphens"
        )))
    }
}

fn zone_name(value: Option<&str>) -> Option<String> {
    let value = value?;
    let name = value.rsplit('/').next().unwrap_or(value);
    path_segment("zone", name).ok()?;
    Some(name.to_string())
}

fn labels_of(value: &Value) -> std::collections::BTreeMap<String, String> {
    let mut tags = std::collections::BTreeMap::new();
    if let Some(obj) = value["labels"].as_object() {
        for (key, item) in obj {
            if let Some(text) = item.as_str() {
                tags.insert(key.clone(), text.to_string());
            }
        }
    }
    tags
}

fn parse_instance(value: &Value) -> Result<CloudInstance, DeployError> {
    let name = value["name"].as_str().unwrap_or("").to_string();
    let mut tags = labels_of(value);
    if tags.get(TAG_MANAGED).map(String::as_str) != Some(MANAGED_VALUE) {
        return Err(DeployError::fail(format!(
            "refusing gcp instance {name} without the managed label"
        )));
    }
    if let Some(zone) = zone_name(value.get("zone").and_then(|v| v.as_str())) {
        tags.insert("echochamber-zone".into(), zone);
    }
    let deployment_id = tags.get(TAG_ID).cloned().unwrap_or_else(|| name.clone());
    let status = value["status"].as_str().unwrap_or("").to_ascii_lowercase();
    let public_ip = value["networkInterfaces"][0]["accessConfigs"][0]["natIP"]
        .as_str()
        .map(str::to_string)
        .filter(|ip| !ip.is_empty());
    Ok(CloudInstance {
        deployment_id,
        provider_id: value["id"].to_string(),
        name,
        state: VmState::from_cloud(&status),
        public_ip,
        rtmp_port: RTMP_PORT,
        control: ControlPlane::GuestLoopback,
        tags,
    })
}

fn urlencoding(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::QualityConfig;
    use axum::extract::State;
    use axum::routing::{delete, get, post};
    use axum::{Json, Router};
    use std::sync::Mutex;

    fn plan() -> InstancePlan {
        let mut tags = super::super::model::managed_tags("ec0123abcd01", 10, 9_000);
        tags.insert("echochamber-expires-at".into(), "9000".into());
        InstancePlan {
            deployment_id: "ec0123abcd01".into(),
            name: "ec0123abcd01".into(),
            region: "us-central1".into(),
            zone: "us-central1-a".into(),
            image: "projects/ubuntu-os-cloud/global/images/family/ubuntu-2404-lts-amd64".into(),
            prebuilt: false,
            size: "e2-small".into(),
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
    fn instance_deletes_itself_at_the_deadline_and_has_no_cloud_scopes() {
        let body = instance_body(&plan());
        assert_eq!(body["scheduling"]["instanceTerminationAction"], "DELETE");
        assert_eq!(body["scheduling"]["maxRunDuration"]["seconds"], "3600");
        assert_eq!(body["scheduling"]["automaticRestart"], false);
        assert!(body["serviceAccounts"][0]["scopes"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(body["labels"][TAG_MANAGED], "true");
        assert!(body["metadata"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["key"] == "block-project-ssh-keys"));
        let text = body.to_string();
        assert!(!text.contains("BEGIN PRIVATE KEY"));
    }

    #[test]
    fn firewalls_allow_only_ingest_and_deny_admin_ports() {
        let allow = ingest_firewall(&plan());
        let deny = deny_admin_firewall(&plan());
        assert_eq!(allow["allowed"][0]["ports"][0], "1935");
        assert_eq!(allow["sourceRanges"][0], "203.0.113.8/32");
        assert!(allow.get("denied").is_none());
        assert_eq!(deny["denied"][0]["ports"][0], "22");
        assert_eq!(deny["denied"][0]["ports"][1], "3389");
        assert!(deny["priority"].as_u64().unwrap() < allow["priority"].as_u64().unwrap());
    }

    struct FixedToken;

    #[async_trait]
    impl GcpTokens for FixedToken {
        async fn bearer(&self) -> Result<String, DeployError> {
            Ok("test-token".into())
        }
    }

    #[derive(Default)]
    struct Fake {
        instances: Vec<Value>,
        firewalls: Vec<Value>,
        auth: Vec<String>,
    }

    #[tokio::test]
    async fn lifecycle_stays_on_localhost() {
        let fake = Arc::new(Mutex::new(Fake::default()));
        let app = Router::new()
            .route(
                "/projects/{project}/global/firewalls",
                post(create_firewall).get(list_firewalls),
            )
            .route(
                "/projects/{project}/global/firewalls/{name}",
                delete(delete_firewall),
            )
            .route(
                "/projects/{project}/zones/{zone}/instances",
                post(create_instance),
            )
            .route(
                "/projects/{project}/zones/{zone}/instances/{name}",
                get(get_instance).delete(delete_instance),
            )
            .route(
                "/projects/{project}/aggregated/instances",
                get(list_instances),
            )
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let provider = GcpProvider::with_tokens(
            format!("http://{addr}"),
            "demo-project",
            Arc::new(FixedToken),
        )
        .unwrap();
        let created = provider.create(&plan()).await.unwrap();
        assert_eq!(created.public_ip.as_deref(), Some("203.0.113.30"));
        assert!(matches!(created.control, ControlPlane::GuestLoopback));
        let described = provider.describe("ec0123abcd01").await.unwrap();
        assert_eq!(described.name, "ec0123abcd01");
        provider.destroy("ec0123abcd01").await.unwrap();
        let auth = fake.lock().unwrap().auth.clone();
        assert!(auth.iter().all(|header| header == "Bearer test-token"));
        assert!(fake.lock().unwrap().instances.is_empty());
        assert!(fake.lock().unwrap().firewalls.is_empty());
    }

    async fn create_firewall(
        State(fake): State<Arc<Mutex<Fake>>>,
        headers: axum::http::HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        note(&fake, &headers);
        fake.lock().unwrap().firewalls.push(body);
        Json(json!({"name": "op-fw", "status": "DONE"}))
    }

    async fn create_instance(
        State(fake): State<Arc<Mutex<Fake>>>,
        headers: axum::http::HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        note(&fake, &headers);
        assert_eq!(body["scheduling"]["instanceTerminationAction"], "DELETE");
        let mut stored = body;
        stored["status"] = json!("RUNNING");
        stored["zone"] = json!("zones/us-central1-a");
        stored["networkInterfaces"] = json!([{
            "accessConfigs": [{ "natIP": "203.0.113.30" }]
        }]);
        fake.lock().unwrap().instances.push(stored);
        Json(json!({"name": "op-vm", "status": "DONE", "zone": "zones/us-central1-a"}))
    }

    async fn get_instance(
        State(fake): State<Arc<Mutex<Fake>>>,
        headers: axum::http::HeaderMap,
    ) -> Json<Value> {
        note(&fake, &headers);
        Json(
            fake.lock()
                .unwrap()
                .instances
                .first()
                .cloned()
                .unwrap_or(json!({})),
        )
    }

    async fn list_instances(
        State(fake): State<Arc<Mutex<Fake>>>,
        headers: axum::http::HeaderMap,
    ) -> Json<Value> {
        note(&fake, &headers);
        let instances = fake.lock().unwrap().instances.clone();
        Json(json!({
            "items": { "zones/us-central1-a": { "instances": instances } }
        }))
    }

    async fn list_firewalls(
        State(fake): State<Arc<Mutex<Fake>>>,
        headers: axum::http::HeaderMap,
    ) -> Json<Value> {
        note(&fake, &headers);
        let items = fake.lock().unwrap().firewalls.clone();
        Json(json!({ "items": items }))
    }

    async fn delete_instance(
        State(fake): State<Arc<Mutex<Fake>>>,
        headers: axum::http::HeaderMap,
    ) -> StatusNoContent {
        note(&fake, &headers);
        fake.lock().unwrap().instances.clear();
        StatusNoContent
    }

    async fn delete_firewall(
        State(fake): State<Arc<Mutex<Fake>>>,
        headers: axum::http::HeaderMap,
    ) -> StatusNoContent {
        note(&fake, &headers);
        fake.lock().unwrap().firewalls.clear();
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
