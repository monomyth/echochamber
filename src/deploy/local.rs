use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::cloud_init::local_watchdog_script;
use super::error::DeployError;
use super::model::{
    relay_config_toml, validate_deployment_id, CloudInstance, ControlPlane, InstancePlan,
    ProviderKind, VmState,
};
use super::provider::CloudProvider;
use crate::process::kill_group;

/// A relay process on this computer. Teardown stops that process. It does not
/// shut the machine down. Home links drop; this is not a hosted ingest.
pub struct LocalProvider {
    binary: PathBuf,
    root: PathBuf,
}

impl LocalProvider {
    pub fn new(binary: impl Into<PathBuf>, root: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            root: root.into(),
        }
    }

    fn dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Meta {
    deployment_id: String,
    pid: u32,
    watchdog_pid: u32,
    rtmp_port: u16,
    control_port: u16,
    control_url: String,
    public_ip: String,
    tags: std::collections::BTreeMap<String, String>,
    state: String,
}

#[async_trait]
impl CloudProvider for LocalProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Local
    }

    async fn create(&self, plan: &InstancePlan) -> Result<CloudInstance, DeployError> {
        let dir = self.dir(&plan.deployment_id);
        fs::create_dir_all(&dir).map_err(|e| DeployError::fail(format!("local dir: {e}")))?;
        let rtmp_port = if port_free(plan.ingest_port) {
            plan.ingest_port
        } else {
            free_port()?
        };
        let control_port = free_port()?;
        let config = relay_config_toml(
            &format!("0.0.0.0:{rtmp_port}"),
            &format!("127.0.0.1:{control_port}"),
            &plan.stream_key,
            &plan.control_token,
            &plan.platforms,
            &plan.quality,
        );
        write_private(&dir.join("config.toml"), config.as_bytes())?;
        let curl_conf = format!(
            "header = \"Authorization: Bearer {}\"\n",
            plan.control_token
        );
        write_private(&dir.join("curl.conf"), curl_conf.as_bytes())?;
        write_private(&dir.join("watch.sh"), local_watchdog_script().as_bytes())?;
        let log = dir.join("relay.log");
        let pidfile = dir.join("relay.pid");
        let watch_pid = dir.join("watch.pid");
        let launcher = dir.join("launch.sh");
        let script = format!(
            "#!/bin/sh\nset -eu\nexport ECHOCHAMBER_DISABLE_DEPLOY=1\nnohup {bin} serve --config {cfg} >> {log} 2>&1 &\necho $! > {pid}\nnohup /bin/sh {watch} {pid} {deadline} {idle} {grace} {curl} {port} >> {log} 2>&1 &\necho $! > {wpid}\n",
            bin = sh_quote(self.binary.to_str().unwrap_or("echochamber")),
            cfg = sh_quote(dir.join("config.toml").to_str().unwrap_or("config.toml")),
            log = sh_quote(log.to_str().unwrap_or("relay.log")),
            pid = sh_quote(pidfile.to_str().unwrap_or("relay.pid")),
            watch = sh_quote(dir.join("watch.sh").to_str().unwrap_or("watch.sh")),
            deadline = plan.expires_at_unix,
            idle = plan.idle_secs,
            grace = plan.connect_grace_secs,
            curl = sh_quote(dir.join("curl.conf").to_str().unwrap_or("curl.conf")),
            port = control_port,
            wpid = sh_quote(watch_pid.to_str().unwrap_or("watch.pid")),
        );
        write_private(&launcher, script.as_bytes())?;
        let status = Command::new("/bin/sh")
            .arg(&launcher)
            .status()
            .map_err(|e| DeployError::fail(format!("local launch: {e}")))?;
        if !status.success() {
            return Err(DeployError::fail(format!("local launch exited {status}")));
        }
        let pid = read_pid(&pidfile)?;
        let watchdog_pid = read_pid(&watch_pid)?;
        let public_ip = preferred_ipv4()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "127.0.0.1".into());
        let control_url = format!("http://127.0.0.1:{control_port}/status");
        let meta = Meta {
            deployment_id: plan.deployment_id.clone(),
            pid,
            watchdog_pid,
            rtmp_port,
            control_port,
            control_url: control_url.clone(),
            public_ip: public_ip.clone(),
            tags: plan.tags.clone(),
            state: "running".into(),
        };
        write_private(
            &dir.join("meta.json"),
            serde_json::to_string_pretty(&meta)
                .unwrap_or_default()
                .as_bytes(),
        )?;
        Ok(instance_from(&meta))
    }

    async fn describe(&self, deployment_id: &str) -> Result<CloudInstance, DeployError> {
        validate_deployment_id(deployment_id)?;
        let mut meta = read_meta(&self.dir(deployment_id))?;
        if meta.state != "terminated" && !alive(meta.pid) {
            meta.state = "stopped".into();
        }
        Ok(instance_from(&meta))
    }

    async fn destroy(&self, deployment_id: &str) -> Result<(), DeployError> {
        validate_deployment_id(deployment_id)?;
        let path = self.dir(deployment_id).join("meta.json");
        if !path.exists() {
            return Ok(());
        }
        let mut meta = read_meta(&self.dir(deployment_id))?;
        kill_group(meta.watchdog_pid, libc::SIGTERM);
        kill_group(meta.pid, libc::SIGTERM);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        if alive(meta.pid) {
            kill_group(meta.pid, libc::SIGKILL);
        }
        if alive(meta.watchdog_pid) {
            kill_group(meta.watchdog_pid, libc::SIGKILL);
        }
        meta.state = "terminated".into();
        write_private(
            &path,
            serde_json::to_string_pretty(&meta)
                .unwrap_or_default()
                .as_bytes(),
        )?;
        Ok(())
    }

    async fn list_managed(&self) -> Result<Vec<CloudInstance>, DeployError> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut found = Vec::new();
        let entries =
            fs::read_dir(&self.root).map_err(|e| DeployError::fail(format!("local list: {e}")))?;
        for entry in entries.flatten() {
            let meta_path = entry.path().join("meta.json");
            if !meta_path.is_file() {
                continue;
            }
            if let Ok(mut meta) = read_meta(&entry.path()) {
                if meta.state == "terminated" {
                    continue;
                }
                if !alive(meta.pid) {
                    meta.state = "stopped".into();
                }
                let inst = instance_from(&meta);
                if super::model::is_managed(&inst.tags) {
                    found.push(inst);
                }
            }
        }
        Ok(found)
    }
}

fn instance_from(meta: &Meta) -> CloudInstance {
    let state = match meta.state.as_str() {
        "running" => VmState::Running,
        "stopped" => VmState::Stopped,
        "terminated" => VmState::Terminated,
        _ => VmState::Failed,
    };
    CloudInstance {
        deployment_id: meta.deployment_id.clone(),
        provider_id: meta.pid.to_string(),
        name: meta.deployment_id.clone(),
        state,
        public_ip: Some(meta.public_ip.clone()),
        rtmp_port: meta.rtmp_port,
        control: ControlPlane::Reachable {
            url: meta.control_url.clone(),
        },
        tags: meta.tags.clone(),
    }
}

fn read_meta(dir: &Path) -> Result<Meta, DeployError> {
    let text = fs::read_to_string(dir.join("meta.json"))
        .map_err(|_| DeployError::missing(format!("no local deployment in {}", dir.display())))?;
    serde_json::from_str(&text).map_err(|e| DeployError::fail(format!("local meta: {e}")))
}

fn read_pid(path: &Path) -> Result<u32, DeployError> {
    let text = fs::read_to_string(path).map_err(|e| DeployError::fail(format!("pid file: {e}")))?;
    text.trim()
        .parse()
        .map_err(|e| DeployError::fail(format!("pid file: {e}")))
}

fn alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn port_free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

fn free_port() -> Result<u16, DeployError> {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        .map_err(|e| DeployError::fail(format!("free port: {e}")))
}

fn preferred_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    match socket.local_addr().ok()? {
        SocketAddr::V4(addr) if !addr.ip().is_unspecified() => Some(*addr.ip()),
        _ => None,
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), DeployError> {
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| DeployError::fail(format!("write {}: {e}", path.display())))?;
    file.write_all(bytes)
        .map_err(|e| DeployError::fail(format!("write {}: {e}", path.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn binary_from_env() -> PathBuf {
    std::env::var("ECHOCHAMBER_RELAY_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::current_exe().unwrap_or_else(|_| PathBuf::from("echochamber"))
        })
}

pub fn state_dir_from_env() -> PathBuf {
    if let Ok(path) = std::env::var("ECHOCHAMBER_LOCAL_STATE") {
        return PathBuf::from(path);
    }
    directories::ProjectDirs::from("", "", "echochamber")
        .map(|d| d.config_dir().join("local"))
        .unwrap_or_else(|| PathBuf::from("local-relays"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::QualityConfig;
    use crate::deploy::model::managed_tags;

    #[test]
    fn spawn_and_destroy_a_stand_in_process() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("standin.sh");
        fs::write(&bin, "#!/bin/sh\nexec sleep 300\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&bin).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&bin, perms).unwrap();
        }
        let provider = LocalProvider::new(&bin, dir.path().join("state"));
        let plan = InstancePlan {
            deployment_id: "ec0123abcd01".into(),
            name: "ec0123abcd01".into(),
            region: "local".into(),
            zone: String::new(),
            image: String::new(),
            prebuilt: false,
            size: String::new(),
            ingest_cidr: "127.0.0.1/32".into(),
            ingest_port: 1935,
            user_data: String::new(),
            tags: managed_tags("ec0123abcd01", 1, 9_000_000_000),
            expires_at_unix: 9_000_000_000,
            created_at_unix: 1,
            stream_key: "abc123".into(),
            control_token: "tok123".into(),
            idle_secs: 600,
            connect_grace_secs: 900,
            ttl_secs: 3600,
            platforms: Default::default(),
            quality: QualityConfig::default(),
            binary_url: None,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let created = runtime.block_on(provider.create(&plan)).unwrap();
        assert!(matches!(created.control, ControlPlane::Reachable { .. }));
        assert!(created.public_ip.is_some());
        let config = fs::read_to_string(dir.path().join("state/ec0123abcd01/config.toml")).unwrap();
        assert!(config.contains("token = \"tok123\""));
        assert!(config.contains("bind = \"127.0.0.1:"));
        let watch = fs::read_to_string(dir.path().join("state/ec0123abcd01/watch.sh")).unwrap();
        assert!(!watch.contains("shutdown"));
        let pid: u32 = created.provider_id.parse().unwrap();
        assert!(alive(pid));
        runtime.block_on(provider.destroy("ec0123abcd01")).unwrap();
        assert!(!alive(pid));
    }
}
