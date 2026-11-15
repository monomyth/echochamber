//! Deploy orchestration against an in-memory provider. No cloud calls.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use echochamber::config::Config;
use echochamber::control::{router, ControlCtx};
use echochamber::deploy::{
    CloudInstance, ControlPlane, DeployRequest, DeployService, DeployStatus, DeploymentRecord,
    FixedClock, FixedFactory, MockEndpoint, MockEvent, MockProvider, ProviderKind, ReapRequest,
    RelaySeed, Store, VmState, DEFAULT_GRACE_SECS, DEFAULT_IDLE_SECS, DEFAULT_TTL_SECS,
};
use echochamber::state::Shared;

fn tags(id: &str, expires: u64) -> std::collections::BTreeMap<String, String> {
    std::collections::BTreeMap::from([
        ("echochamber-managed".into(), "true".into()),
        ("echochamber-deployment-id".into(), id.into()),
        ("echochamber-expires-at".into(), expires.to_string()),
        ("echochamber-created-at".into(), "1".into()),
    ])
}

fn instance(id: &str, state: VmState, expires: u64) -> CloudInstance {
    CloudInstance {
        deployment_id: id.into(),
        provider_id: id.into(),
        name: id.into(),
        state,
        public_ip: Some("203.0.113.9".into()),
        rtmp_port: 1935,
        control: ControlPlane::GuestLoopback,
        tags: tags(id, expires),
    }
}

async fn status_server(ready: Arc<AtomicBool>) -> SocketAddr {
    use axum::routing::get;
    use axum::{Json, Router};
    use serde_json::json;

    let app = Router::new().route(
        "/status",
        get(move || {
            let ready = Arc::clone(&ready);
            async move {
                if ready.load(Ordering::SeqCst) {
                    (
                        axum::http::StatusCode::OK,
                        Json(json!({"ingest": {"publishing": false}})),
                    )
                } else {
                    (
                        axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({"error": "starting"})),
                    )
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

fn service(provider: Arc<MockProvider>, poll_ms: u64) -> DeployService {
    let dir = tempfile::tempdir().unwrap();
    // Leak the tempdir so the store file outlives this function. Tests are short.
    let path = dir.path().join("deployments.json");
    std::mem::forget(dir);
    DeployService::new(
        Arc::new(FixedFactory { provider }),
        Store::new(path),
        Arc::new(echochamber::deploy::SystemClock),
        Duration::from_millis(poll_ms),
    )
}

fn request(kind: ProviderKind, timeout: u64) -> DeployRequest {
    DeployRequest {
        provider: kind,
        region: if kind == ProviderKind::Local {
            "local".into()
        } else {
            "fsn1".into()
        },
        zone: None,
        ttl_secs: DEFAULT_TTL_SECS,
        idle_secs: DEFAULT_IDLE_SECS,
        connect_grace_secs: DEFAULT_GRACE_SECS,
        image: None,
        prebuilt: false,
        size: None,
        ingest_cidr: "0.0.0.0/0".into(),
        binary_url: None,
        ready_timeout_secs: timeout,
    }
}

#[tokio::test]
async fn ingest_url_is_withheld_until_rtmp_and_control_both_answer() {
    let flag = Arc::new(AtomicBool::new(false));
    let http = status_server(Arc::clone(&flag)).await;
    let rtmp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rtmp_addr = rtmp.local_addr().unwrap();
    let mock = Arc::new(
        MockProvider::new(ProviderKind::Hetzner).endpoint(MockEndpoint {
            ip: "127.0.0.1".into(),
            rtmp_port: rtmp_addr.port(),
            control: ControlPlane::Reachable {
                url: format!("http://{http}/status"),
            },
        }),
    );
    let svc = service(Arc::clone(&mock), 20);
    let flag2 = Arc::clone(&flag);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        flag2.store(true, Ordering::SeqCst);
    });
    let record = svc
        .deploy(request(ProviderKind::Hetzner, 3), RelaySeed::default())
        .await
        .unwrap();
    assert_eq!(record.status, DeployStatus::Ready);
    let url = record.ingest_url().unwrap();
    assert!(url.starts_with(&format!(
        "rtmp://127.0.0.1:{}/echochamber/",
        rtmp_addr.port()
    )));
    assert_ne!(record.stream_key.as_deref(), Some("live"));
    let events = mock.events();
    assert!(events.iter().any(|e| matches!(e, MockEvent::Restrict(_))));
    assert!(!events.iter().any(|e| matches!(e, MockEvent::Destroy(_))));
    let plan = mock.plans().pop().unwrap();
    assert_eq!(
        plan.tags.get("echochamber-managed").map(String::as_str),
        Some("true")
    );
    assert!(plan
        .user_data
        .contains("iptables -I INPUT -p tcp --dport 1935 -j DROP"));
    assert!(!plan.user_data.contains(&plan.control_token));
    drop(rtmp);
}

#[tokio::test]
async fn failed_health_check_destroys_and_returns_no_url() {
    let rtmp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = rtmp.local_addr().unwrap().port();
    let mock = Arc::new(
        MockProvider::new(ProviderKind::AwsLightsail).endpoint(MockEndpoint {
            ip: "127.0.0.1".into(),
            rtmp_port: port,
            control: ControlPlane::Reachable {
                url: "http://127.0.0.1:1/status".into(),
            },
        }),
    );
    let svc = service(Arc::clone(&mock), 20);
    let err = svc
        .deploy(request(ProviderKind::AwsLightsail, 1), RelaySeed::default())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not ready") || err.to_string().contains("control"));
    assert!(mock
        .events()
        .iter()
        .any(|e| matches!(e, MockEvent::Destroy(_))));
    let stored = svc.list().unwrap();
    assert_eq!(stored.len(), 1);
    assert!(stored[0].ingest_url().is_none());
    assert_eq!(stored[0].status, DeployStatus::Error);
    drop(rtmp);
}

#[tokio::test]
async fn guest_gate_still_requires_the_ingest_port() {
    let mock = Arc::new(
        MockProvider::new(ProviderKind::Hetzner).endpoint(MockEndpoint {
            ip: "127.0.0.1".into(),
            rtmp_port: 1,
            control: ControlPlane::GuestLoopback,
        }),
    );
    let svc = service(Arc::clone(&mock), 20);
    assert!(svc
        .deploy(request(ProviderKind::Hetzner, 1), RelaySeed::default())
        .await
        .is_err());
    assert!(mock
        .events()
        .iter()
        .any(|e| matches!(e, MockEvent::Destroy(_))));
}

#[tokio::test]
async fn reaper_deletes_expired_and_stopped_and_skips_the_rest() {
    let mock = Arc::new(MockProvider::new(ProviderKind::Hetzner));
    mock.insert(instance("ec1111111111", VmState::Running, 50));
    mock.insert(instance("ec2222222222", VmState::Running, 5_000));
    mock.insert(instance("ec3333333333", VmState::Stopped, 5_000));
    let mut foreign = instance("ec4444444444", VmState::Running, 1);
    foreign
        .tags
        .insert("echochamber-managed".into(), "no".into());
    mock.insert(foreign);
    let dir = tempfile::tempdir().unwrap();
    let svc = DeployService::new(
        Arc::new(FixedFactory {
            provider: Arc::clone(&mock),
        }),
        Store::new(dir.path().join("deployments.json")),
        Arc::new(FixedClock(100)),
        Duration::from_millis(20),
    );
    let dry = svc
        .reap(ReapRequest {
            provider: ProviderKind::Hetzner,
            region: "fsn1".into(),
            dry_run: true,
        })
        .await
        .unwrap();
    assert!(dry.deleted.iter().any(|id| id == "ec1111111111"));
    assert!(dry.deleted.iter().any(|id| id == "ec3333333333"));
    assert!(dry.kept.iter().any(|id| id == "ec2222222222"));
    assert!(!dry.deleted.iter().any(|id| id == "ec4444444444"));
    assert!(!mock
        .events()
        .iter()
        .any(|e| matches!(e, MockEvent::Destroy(_))));
    let real = svc
        .reap(ReapRequest {
            provider: ProviderKind::Hetzner,
            region: "fsn1".into(),
            dry_run: false,
        })
        .await
        .unwrap();
    assert_eq!(real.deleted.len(), 2);
    let destroyed: Vec<_> = mock
        .events()
        .into_iter()
        .filter(|e| matches!(e, MockEvent::Destroy(_)))
        .collect();
    assert_eq!(destroyed.len(), 2);
}

#[tokio::test]
async fn http_deploy_rejects_credential_fields_and_serves_the_page() {
    let flag = Arc::new(AtomicBool::new(true));
    let http = status_server(flag).await;
    let rtmp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = rtmp.local_addr().unwrap().port();
    let mock = Arc::new(
        MockProvider::new(ProviderKind::Local).endpoint(MockEndpoint {
            ip: "127.0.0.1".into(),
            rtmp_port: port,
            control: ControlPlane::Reachable {
                url: format!("http://{http}/status"),
            },
        }),
    );
    let dir = tempfile::tempdir().unwrap();
    let deploy = Arc::new(DeployService::new(
        Arc::new(FixedFactory { provider: mock }),
        Store::new(dir.path().join("deployments.json")),
        Arc::new(echochamber::deploy::SystemClock),
        Duration::from_millis(20),
    ));
    let mut cfg = Config::default();
    cfg.control.token = "page-token".into();
    let shared = Shared::new(cfg, std::path::PathBuf::from("config.toml"));
    let app = router(ControlCtx {
        shared,
        deploy: Some(deploy),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = reqwest::Client::new();
    let page = client
        .get(format!("http://{addr}/cloud"))
        .send()
        .await
        .unwrap();
    assert!(page.status().is_success());
    let html = page.text().await.unwrap();
    assert!(html.contains("Deploy relay"));
    assert!(html.contains("Hetzner"));
    let denied = client
        .post(format!("http://{addr}/deploy"))
        .json(&serde_json::json!({
            "provider": "local",
            "region": "local",
            "aws_secret_access_key": "should-not-be-accepted"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
    let bad = client
        .post(format!("http://{addr}/deploy"))
        .header("authorization", "Bearer page-token")
        .json(&serde_json::json!({
            "provider": "local",
            "region": "local",
            "hetzner_token": "nope"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 422);
    let ok = client
        .post(format!("http://{addr}/deploy"))
        .header("authorization", "Bearer page-token")
        .json(&serde_json::json!({
            "provider": "local",
            "region": "local",
            "ready_timeout_secs": 3
        }))
        .send()
        .await
        .unwrap();
    assert!(ok.status().is_success(), "{}", ok.text().await.unwrap());
    drop(rtmp);
}

#[test]
fn lightsail_policy_is_tagged_and_not_a_general_admin_policy() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("deploy/iam/aws-lightsail-policy.json");
    let text = std::fs::read_to_string(path).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    let blob = value.to_string();
    assert!(blob.contains("lightsail:CreateInstances"));
    assert!(blob.contains("lightsail:DeleteInstance"));
    assert!(blob.contains("echochamber-managed"));
    assert!(!blob.contains("ec2:"));
    assert!(!blob.contains("s3:"));
    assert!(!blob.contains("iam:"));
    assert!(!blob.contains("gcp"));
}

#[test]
fn deployment_record_hides_the_url_until_ready() {
    let mut record = DeploymentRecord {
        id: "ec0123abcd01".into(),
        provider: ProviderKind::Local,
        region: "local".into(),
        zone: String::new(),
        name: "ec0123abcd01".into(),
        status: DeployStatus::Provisioning,
        public_ip: Some("127.0.0.1".into()),
        ingest_server: Some("rtmp://127.0.0.1:1935/echochamber".into()),
        stream_key: Some("abc".into()),
        expires_at_unix: 1,
        created_at_unix: 1,
        error: None,
    };
    assert!(record.ingest_url().is_none());
    record.status = DeployStatus::Ready;
    assert_eq!(
        record.ingest_url().as_deref(),
        Some("rtmp://127.0.0.1:1935/echochamber/abc")
    );
}
