use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::error::DeployError;
use super::model::{
    validate_deployment_id, CloudInstance, ControlPlane, InstancePlan, ProviderKind, VmState,
    MANAGED_VALUE, RTMP_PORT, TAG_FIREWALL, TAG_ID, TAG_MANAGED,
};
use super::provider::CloudProvider;

pub const DEFAULT_BASE: &str = "https://api.hetzner.cloud/v1";

pub struct HetznerProvider {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl HetznerProvider {
    pub fn new(token: impl Into<String>) -> Result<Self, DeployError> {
        let endpoint =
            std::env::var("HCLOUD_ENDPOINT").unwrap_or_else(|_| DEFAULT_BASE.to_string());
        Self::with_endpoint(endpoint, token)
    }

    pub fn with_endpoint(
        base: impl Into<String>,
        token: impl Into<String>,
    ) -> Result<Self, DeployError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| DeployError::fail(format!("hetzner client: {e}")))?;
        Ok(Self {
            http,
            base: base.into().trim_end_matches('/').to_string(),
            token: token.into(),
        })
    }
}

pub fn load_token() -> Result<String, DeployError> {
    for key in ["HETZNER_API_TOKEN", "HCLOUD_TOKEN"] {
        if let Ok(value) = std::env::var(key) {
            let value = value.trim();
            if !value.is_empty() {
                return Ok(value.to_string());
            }
        }
    }
    let path = std::env::var("HCLOUD_CONFIG")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            directories::BaseDirs::new()
                .map(|d| d.home_dir().join(".config/hcloud/cli.toml"))
                .unwrap_or_else(|| std::path::PathBuf::from(".config/hcloud/cli.toml"))
        });
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Some(token) = token_from_toml(&text) {
            return Ok(token);
        }
    }
    Err(DeployError::fail(
        "no Hetzner token on this machine. Set HETZNER_API_TOKEN or HCLOUD_TOKEN. Create a Read & Write token in a project that contains only relay servers.",
    ))
}

pub fn token_from_toml(text: &str) -> Option<String> {
    let value: Value = toml::from_str(text).ok()?;
    if let Some(token) = value.get("token").and_then(|v| v.as_str()) {
        let token = token.trim();
        if !token.is_empty() {
            return Some(token.to_string());
        }
    }
    let active = value
        .get("active_context")
        .and_then(|v| v.as_str())
        .unwrap_or("default");
    let contexts = value.get("contexts")?.as_array()?;
    for ctx in contexts {
        if ctx.get("name").and_then(|v| v.as_str()) == Some(active) {
            if let Some(token) = ctx.get("token").and_then(|v| v.as_str()) {
                let token = token.trim();
                if !token.is_empty() {
                    return Some(token.to_string());
                }
            }
        }
    }
    None
}

pub fn firewall_body(plan: &InstancePlan) -> Value {
    json!({
        "name": format!("{}-fw", plan.name),
        "labels": plan.tags,
        "rules": [{
            "direction": "in",
            "protocol": "tcp",
            "port": plan.ingest_port.to_string(),
            "source_ips": [plan.ingest_cidr],
        }],
    })
}

pub fn server_body(plan: &InstancePlan, firewall_id: u64) -> Value {
    let image = if plan.image.chars().all(|c| c.is_ascii_digit()) && !plan.image.is_empty() {
        json!(plan.image.parse::<u64>().unwrap_or(0))
    } else {
        json!(plan.image)
    };
    json!({
        "name": plan.name,
        "server_type": plan.size,
        "image": image,
        "location": plan.region,
        "start_after_create": true,
        "ssh_keys": [],
        "user_data": plan.user_data,
        "labels": plan.tags,
        "public_net": {
            "enable_ipv4": true,
            "enable_ipv6": false
        },
        "firewalls": [{ "firewall": firewall_id }],
    })
}

#[async_trait]
impl CloudProvider for HetznerProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Hetzner
    }

    async fn create(&self, plan: &InstancePlan) -> Result<CloudInstance, DeployError> {
        let created = self.post("/firewalls", &firewall_body(plan)).await?;
        let firewall_id = created["firewall"]["id"]
            .as_u64()
            .ok_or_else(|| DeployError::fail("hetzner firewall response had no id"))?;
        let mut labeled = plan.clone();
        labeled
            .tags
            .insert(TAG_FIREWALL.to_string(), firewall_id.to_string());
        match self
            .post("/servers", &server_body(&labeled, firewall_id))
            .await
        {
            Ok(body) => parse_server(&body["server"]),
            Err(err) => {
                let _ = self.delete_ok(&format!("/firewalls/{firewall_id}")).await;
                Err(err)
            }
        }
    }

    async fn describe(&self, deployment_id: &str) -> Result<CloudInstance, DeployError> {
        validate_deployment_id(deployment_id)?;
        self.find_server(deployment_id)
            .await?
            .ok_or_else(|| DeployError::missing(format!("no hetzner server {deployment_id}")))
    }

    async fn destroy(&self, deployment_id: &str) -> Result<(), DeployError> {
        validate_deployment_id(deployment_id)?;
        if let Some(server) = self.find_server(deployment_id).await? {
            let id = numeric_id(&server.provider_id)?;
            self.delete_ok(&format!("/servers/{id}")).await?;
        }
        for firewall_id in self.firewall_ids(deployment_id).await? {
            self.delete_firewall(firewall_id).await?;
        }
        Ok(())
    }

    async fn list_managed(&self) -> Result<Vec<CloudInstance>, DeployError> {
        let mut found = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let servers = self
            .list_labeled("/servers", "servers", "echochamber-managed=true")
            .await?;
        for server in servers {
            if let Ok(inst) = parse_server(&server) {
                seen.insert(inst.deployment_id.clone());
                found.push(inst);
            }
        }
        let firewalls = self
            .list_labeled("/firewalls", "firewalls", "echochamber-managed=true")
            .await?;
        for firewall in firewalls {
            let labels = labels_of(&firewall);
            let Some(id) = labels.get(TAG_ID) else {
                continue;
            };
            if seen.contains(id) {
                continue;
            }
            let fw_id = firewall["id"].as_u64().unwrap_or(0);
            found.push(CloudInstance {
                deployment_id: id.clone(),
                provider_id: format!("fw:{fw_id}"),
                name: firewall["name"].as_str().unwrap_or("").to_string(),
                state: VmState::Terminated,
                public_ip: None,
                rtmp_port: RTMP_PORT,
                control: ControlPlane::GuestLoopback,
                tags: labels,
            });
        }
        Ok(found)
    }

    async fn restrict_ingress(&self, deployment_id: &str) -> Result<(), DeployError> {
        let ids = self.firewall_ids(deployment_id).await?;
        if ids.is_empty() {
            return Err(DeployError::fail(format!(
                "hetzner deployment {deployment_id} has no firewall"
            )));
        }
        for id in ids {
            let current = self.get(&format!("/firewalls/{id}")).await?;
            let sources = current["firewall"]["rules"]
                .as_array()
                .and_then(|rules| rules.iter().find(|rule| rule["port"] == "1935"))
                .and_then(|rule| rule["source_ips"].as_array())
                .cloned()
                .unwrap_or_else(|| vec![json!("0.0.0.0/0")]);
            self.post(
                &format!("/firewalls/{id}/actions/set_rules"),
                &json!({
                    "rules": [{
                        "direction": "in",
                        "protocol": "tcp",
                        "port": "1935",
                        "source_ips": sources,
                    }]
                }),
            )
            .await?;
        }
        Ok(())
    }
}

impl HetznerProvider {
    async fn find_server(&self, deployment_id: &str) -> Result<Option<CloudInstance>, DeployError> {
        let selector = format!("{TAG_ID}={deployment_id}");
        let servers = self.list_labeled("/servers", "servers", &selector).await?;
        let mut parsed = Vec::new();
        for server in servers {
            parsed.push(parse_server(&server)?);
        }
        if parsed.len() > 1 {
            return Err(DeployError::fail(format!(
                "more than one hetzner server is labeled {deployment_id}"
            )));
        }
        Ok(parsed.pop())
    }

    async fn firewall_ids(&self, deployment_id: &str) -> Result<Vec<u64>, DeployError> {
        let selector = format!("{TAG_ID}={deployment_id}");
        let firewalls = self
            .list_labeled("/firewalls", "firewalls", &selector)
            .await?;
        let mut ids = Vec::new();
        for firewall in firewalls {
            if let Some(id) = firewall["id"].as_u64() {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    async fn delete_firewall(&self, id: u64) -> Result<(), DeployError> {
        let path = format!("/firewalls/{id}");
        for _ in 0..5 {
            match self.delete_ok(&path).await {
                Ok(()) => return Ok(()),
                Err(err) => {
                    let text = err.to_string();
                    if text.contains("in use") || text.contains("resource_in_use") {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                    return Err(err);
                }
            }
        }
        self.delete_ok(&path).await
    }

    async fn list_labeled(
        &self,
        path: &str,
        key: &str,
        selector: &str,
    ) -> Result<Vec<Value>, DeployError> {
        let mut page = 1u64;
        let mut all = Vec::new();
        loop {
            let url = format!(
                "{path}?page={page}&label_selector={}",
                urlencoding_light(selector)
            );
            let body = self.get(&url).await?;
            if let Some(items) = body[key].as_array() {
                all.extend(items.iter().cloned());
            }
            match body["meta"]["pagination"]["next_page"].as_u64() {
                Some(next) if next != page && next < 100 => page = next,
                _ => break,
            }
        }
        Ok(all)
    }

    async fn get(&self, path: &str) -> Result<Value, DeployError> {
        self.send(self.request(reqwest::Method::GET, path)).await
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value, DeployError> {
        self.send(self.request(reqwest::Method::POST, path).json(body))
            .await
    }

    async fn delete_ok(&self, path: &str) -> Result<(), DeployError> {
        let response = self
            .request(reqwest::Method::DELETE, path)
            .send()
            .await
            .map_err(|e| DeployError::fail(format!("hetzner: {e}")))?;
        if response.status().as_u16() == 404 || response.status().is_success() {
            return Ok(());
        }
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        Err(DeployError::fail(hetzner_error(status.as_u16(), &text)))
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token)
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Value, DeployError> {
        let response = request
            .send()
            .await
            .map_err(|e| DeployError::fail(format!("hetzner: {e}")))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(DeployError::fail(hetzner_error(status.as_u16(), &text)));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| DeployError::fail(format!("hetzner json: {e}")))
    }
}

fn hetzner_error(status: u16, body: &str) -> String {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "request failed".into());
    format!("hetzner http {status}: {message}")
}

fn numeric_id(id: &str) -> Result<u64, DeployError> {
    id.parse()
        .map_err(|_| DeployError::fail(format!("refusing non-numeric hetzner id {id:?}")))
}

fn labels_of(value: &Value) -> BTreeMap<String, String> {
    let mut tags = BTreeMap::new();
    if let Some(obj) = value["labels"].as_object() {
        for (key, item) in obj {
            if let Some(text) = item.as_str() {
                tags.insert(key.clone(), text.to_string());
            }
        }
    }
    tags
}

fn parse_server(value: &Value) -> Result<CloudInstance, DeployError> {
    let id = value["id"]
        .as_u64()
        .ok_or_else(|| DeployError::fail("hetzner server has no id"))?;
    let name = value["name"].as_str().unwrap_or("").to_string();
    let tags = labels_of(value);
    if tags.get(TAG_MANAGED).map(String::as_str) != Some(MANAGED_VALUE) {
        return Err(DeployError::fail(format!(
            "refusing hetzner server {name} without the managed label"
        )));
    }
    let deployment_id = tags.get(TAG_ID).cloned().unwrap_or_else(|| name.clone());
    let state = VmState::from_cloud(value["status"].as_str().unwrap_or(""));
    let public_ip = value["public_net"]["ipv4"]["ip"]
        .as_str()
        .map(|ip| ip.to_string())
        .filter(|ip| !ip.is_empty());
    Ok(CloudInstance {
        deployment_id,
        provider_id: id.to_string(),
        name,
        state,
        public_ip,
        rtmp_port: RTMP_PORT,
        control: ControlPlane::GuestLoopback,
        tags,
    })
}

fn urlencoding_light(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::QualityConfig;
    use crate::deploy::model::managed_tags;
    use axum::extract::State;
    use axum::routing::{delete, get, post};
    use axum::{Json, Router};
    use std::sync::{Arc, Mutex};

    fn plan() -> InstancePlan {
        InstancePlan {
            deployment_id: "ec0123abcd01".into(),
            name: "ec0123abcd01".into(),
            region: "fsn1".into(),
            zone: "fsn1".into(),
            image: "ubuntu-24.04".into(),
            prebuilt: false,
            size: "cx23".into(),
            ingest_cidr: "203.0.113.8/32".into(),
            ingest_port: 1935,
            user_data: "#!/bin/bash\necho hi\n".into(),
            tags: managed_tags("ec0123abcd01", 10, 99),
            expires_at_unix: 99,
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
    fn firewall_allows_only_the_ingest_port_and_carries_labels() {
        let body = firewall_body(&plan());
        let rules = body["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0]["protocol"], "tcp");
        assert_eq!(rules[0]["port"], "1935");
        assert_eq!(rules[0]["source_ips"][0], "203.0.113.8/32");
        assert_eq!(body["labels"][TAG_MANAGED], MANAGED_VALUE);
        assert!(body.get("ssh_keys").is_none());
        let text = body.to_string();
        assert!(!text.contains("Bearer"));
        assert!(!text.contains("secret-token"));
    }

    #[test]
    fn server_body_disables_ipv6_and_ssh_keys() {
        let body = server_body(&plan(), 42);
        assert_eq!(body["public_net"]["enable_ipv6"], false);
        assert_eq!(body["ssh_keys"].as_array().unwrap().len(), 0);
        assert_eq!(body["firewalls"][0]["firewall"], 42);
        assert_eq!(body["location"], "fsn1");
        assert!(body["user_data"].as_str().unwrap().contains("#!/bin/bash"));
    }

    #[test]
    fn token_parser_reads_active_context_only() {
        let text = r#"
active_context = "relay"
[[contexts]]
name = "other"
token = "not-this"
[[contexts]]
name = "relay"
token = "use-this"
"#;
        assert_eq!(token_from_toml(text).as_deref(), Some("use-this"));
    }

    #[derive(Default)]
    struct Fake {
        servers: Vec<Value>,
        firewalls: Vec<Value>,
        auth: Vec<String>,
    }

    #[tokio::test]
    async fn lifecycle_stays_on_localhost() {
        let fake = Arc::new(Mutex::new(Fake::default()));
        let app = Router::new()
            .route("/firewalls", post(create_firewall).get(list_firewalls))
            .route("/firewalls/{id}", get(get_firewall).delete(delete_firewall))
            .route("/firewalls/{id}/actions/set_rules", post(set_rules))
            .route("/servers", post(create_server).get(list_servers))
            .route("/servers/{id}", delete(delete_server))
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let provider =
            HetznerProvider::with_endpoint(format!("http://{addr}"), "test-token").unwrap();
        let created = provider.create(&plan()).await.unwrap();
        assert_eq!(created.public_ip.as_deref(), Some("203.0.113.10"));
        assert!(matches!(created.control, ControlPlane::GuestLoopback));
        assert_eq!(
            created.tags.get(TAG_MANAGED).map(String::as_str),
            Some("true")
        );
        let described = provider.describe("ec0123abcd01").await.unwrap();
        assert_eq!(described.provider_id, created.provider_id);
        provider.restrict_ingress("ec0123abcd01").await.unwrap();
        provider.destroy("ec0123abcd01").await.unwrap();
        let auth = fake.lock().unwrap().auth.clone();
        assert!(auth.iter().all(|header| header == "Bearer test-token"));
        assert!(fake.lock().unwrap().servers.is_empty());
        assert!(fake.lock().unwrap().firewalls.is_empty());
    }

    async fn create_firewall(
        State(fake): State<Arc<Mutex<Fake>>>,
        headers: axum::http::HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        note_auth(&fake, &headers);
        let firewall = json!({
            "id": 9,
            "name": body["name"],
            "labels": body["labels"],
            "rules": body["rules"],
        });
        fake.lock().unwrap().firewalls.push(firewall.clone());
        Json(json!({ "firewall": firewall }))
    }

    async fn create_server(
        State(fake): State<Arc<Mutex<Fake>>>,
        headers: axum::http::HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        note_auth(&fake, &headers);
        assert_eq!(body["ssh_keys"].as_array().unwrap().len(), 0);
        assert_eq!(body["public_net"]["enable_ipv6"], false);
        let server = json!({
            "id": 3,
            "name": body["name"],
            "status": "running",
            "labels": body["labels"],
            "public_net": { "ipv4": { "ip": "203.0.113.10" } }
        });
        fake.lock().unwrap().servers.push(server.clone());
        Json(json!({ "server": server }))
    }

    async fn list_servers(
        State(fake): State<Arc<Mutex<Fake>>>,
        headers: axum::http::HeaderMap,
    ) -> Json<Value> {
        note_auth(&fake, &headers);
        let servers = fake.lock().unwrap().servers.clone();
        Json(json!({ "servers": servers }))
    }

    async fn list_firewalls(
        State(fake): State<Arc<Mutex<Fake>>>,
        headers: axum::http::HeaderMap,
    ) -> Json<Value> {
        note_auth(&fake, &headers);
        let firewalls = fake.lock().unwrap().firewalls.clone();
        Json(json!({ "firewalls": firewalls }))
    }

    async fn delete_server(
        State(fake): State<Arc<Mutex<Fake>>>,
        headers: axum::http::HeaderMap,
    ) -> StatusNoContent {
        note_auth(&fake, &headers);
        fake.lock().unwrap().servers.clear();
        StatusNoContent
    }

    async fn delete_firewall(
        State(fake): State<Arc<Mutex<Fake>>>,
        headers: axum::http::HeaderMap,
    ) -> StatusNoContent {
        note_auth(&fake, &headers);
        fake.lock().unwrap().firewalls.clear();
        StatusNoContent
    }

    async fn set_rules(
        State(fake): State<Arc<Mutex<Fake>>>,
        headers: axum::http::HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        note_auth(&fake, &headers);
        let rules = body["rules"].as_array().cloned().unwrap_or_default();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0]["port"], "1935");
        assert_eq!(rules[0]["source_ips"][0], "203.0.113.8/32");
        Json(json!({ "action": { "status": "success" } }))
    }

    async fn get_firewall(
        State(fake): State<Arc<Mutex<Fake>>>,
        headers: axum::http::HeaderMap,
    ) -> Json<Value> {
        note_auth(&fake, &headers);
        let firewall = fake
            .lock()
            .unwrap()
            .firewalls
            .first()
            .cloned()
            .unwrap_or_else(|| json!({}));
        Json(json!({ "firewall": firewall }))
    }

    fn note_auth(fake: &Arc<Mutex<Fake>>, headers: &axum::http::HeaderMap) {
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
