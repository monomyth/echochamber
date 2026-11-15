use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpStream;

use super::error::DeployError;
use super::model::{CloudInstance, ControlPlane, VmState};
use super::provider::CloudProvider;

/// TCP accept on the ingest port, and an HTTP `/status` from the control
/// endpoint when this controller can reach it.
///
/// On a cloud guest the control port is not in the firewall. The guest holds
/// the ingest port closed until its own `/status` request returns the relay
/// JSON, so a successful TCP accept is the controller-visible half of that
/// check. `cloud_init` is what performs the HTTP half on the guest.
pub async fn probe_ready(inst: &CloudInstance, token: &str) -> Result<(), DeployError> {
    let ip = inst
        .public_ip
        .as_deref()
        .filter(|ip| !ip.is_empty())
        .ok_or_else(|| DeployError::fail("instance has no address yet"))?;
    let rtmp: SocketAddr = format!("{ip}:{}", inst.rtmp_port)
        .parse()
        .map_err(|e| DeployError::fail(format!("ingest address: {e}")))?;
    probe_tcp(rtmp).await?;
    match &inst.control {
        ControlPlane::Reachable { url } => probe_http(url, token).await,
        ControlPlane::GuestLoopback => Ok(()),
    }
}

pub async fn probe_tcp(addr: SocketAddr) -> Result<(), DeployError> {
    match tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(addr)).await {
        Ok(Ok(_stream)) => Ok(()),
        Ok(Err(e)) => Err(DeployError::fail(format!("rtmp port: {e}"))),
        Err(_) => Err(DeployError::fail("rtmp port timed out")),
    }
}

pub async fn probe_http(url: &str, token: &str) -> Result<(), DeployError> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(3))
        .no_proxy()
        .build()
        .map_err(|e| DeployError::fail(format!("http client: {e}")))?;
    let response = client
        .get(url)
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| DeployError::fail(format!("control endpoint: {e}")))?;
    let status = response.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(DeployError::fail(
            "control endpoint rejected the deployment token",
        ));
    }
    if !status.is_success() {
        return Err(DeployError::fail(format!(
            "control endpoint returned {status}"
        )));
    }
    let body = response
        .text()
        .await
        .map_err(|e| DeployError::fail(format!("control body: {e}")))?;
    if !body.contains("\"publishing\"") {
        return Err(DeployError::fail(
            "control endpoint response was not the relay status",
        ));
    }
    Ok(())
}

pub async fn wait_until_ready(
    provider: &dyn CloudProvider,
    deployment_id: &str,
    token: &str,
    poll: Duration,
    timeout: Duration,
) -> Result<CloudInstance, DeployError> {
    let started = tokio::time::Instant::now();
    loop {
        if started.elapsed() > timeout {
            return Err(DeployError::fail(format!(
                "relay was not ready within {}s",
                timeout.as_secs()
            )));
        }
        let inst = provider.describe(deployment_id).await?;
        match inst.state {
            VmState::Failed | VmState::Terminated => {
                return Err(DeployError::fail(format!(
                    "instance entered {:?}",
                    inst.state
                )));
            }
            VmState::Running => {
                if probe_ready(&inst, token).await.is_ok() {
                    return Ok(inst);
                }
            }
            VmState::Pending | VmState::Stopped => {}
        }
        tokio::time::sleep(poll).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use axum::{Json, Router};
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tokio::net::TcpListener;

    async fn serve_status(ok: Arc<AtomicBool>, token: &'static str) -> SocketAddr {
        let app = Router::new().route(
            "/status",
            get(move |headers: axum::http::HeaderMap| {
                let ok = Arc::clone(&ok);
                async move {
                    let presented = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    if presented != format!("Bearer {token}") {
                        return (
                            axum::http::StatusCode::UNAUTHORIZED,
                            Json(json!({"error": "no"})),
                        );
                    }
                    if !ok.load(Ordering::SeqCst) {
                        return (
                            axum::http::StatusCode::SERVICE_UNAVAILABLE,
                            Json(json!({"error": "not yet"})),
                        );
                    }
                    (
                        axum::http::StatusCode::OK,
                        Json(json!({"ingest": {"publishing": false}})),
                    )
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    #[tokio::test]
    async fn http_probe_waits_for_the_relay_status_and_the_token() {
        let flag = Arc::new(AtomicBool::new(false));
        let addr = serve_status(Arc::clone(&flag), "secret-token").await;
        let url = format!("http://{addr}/status");
        assert!(probe_http(&url, "nope").await.is_err());
        assert!(probe_http(&url, "secret-token").await.is_err());
        flag.store(true, Ordering::SeqCst);
        probe_http(&url, "secret-token").await.unwrap();
    }

    #[tokio::test]
    async fn tcp_probe_requires_an_accept() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        probe_tcp(addr).await.unwrap();
        drop(listener);
        assert!(probe_tcp(addr).await.is_err());
    }
}
