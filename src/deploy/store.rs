use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::error::DeployError;
use super::model::DeploymentRecord;

#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct FileShape {
    deployments: Vec<DeploymentRecord>,
}

impl Store {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn upsert(&self, record: &DeploymentRecord) -> Result<(), DeployError> {
        self.with_lock(|state| {
            if let Some(existing) = state.deployments.iter_mut().find(|row| row.id == record.id) {
                *existing = record.clone();
            } else {
                state.deployments.push(record.clone());
            }
            Ok(())
        })
    }

    pub fn get(&self, id: &str) -> Result<Option<DeploymentRecord>, DeployError> {
        self.with_lock(|state| Ok(state.deployments.iter().find(|row| row.id == id).cloned()))
    }

    pub fn list(&self) -> Result<Vec<DeploymentRecord>, DeployError> {
        self.with_lock(|state| Ok(state.deployments.clone()))
    }

    pub fn mark_destroyed(&self, id: &str) -> Result<(), DeployError> {
        self.with_lock(|state| {
            if let Some(row) = state.deployments.iter_mut().find(|row| row.id == id) {
                row.status = super::model::DeployStatus::Destroyed;
                row.stream_key = None;
            }
            Ok(())
        })
    }

    fn with_lock<T>(
        &self,
        body: impl FnOnce(&mut FileShape) -> Result<T, DeployError>,
    ) -> Result<T, DeployError> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(|e| DeployError::fail(format!("state dir: {e}")))?;
            }
        }
        let lock_path = lock_path(&self.path);
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&lock_path)
            .map_err(|e| DeployError::fail(format!("state lock: {e}")))?;
        unsafe {
            libc::flock(lock.as_raw_fd(), libc::LOCK_EX);
        }
        let result = (|| {
            let mut state = read_state(&self.path)?;
            let value = body(&mut state)?;
            write_state(&self.path, &state)?;
            Ok(value)
        })();
        unsafe {
            libc::flock(lock.as_raw_fd(), libc::LOCK_UN);
        }
        result
    }
}

fn lock_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| "deployments.json".into());
    name.push(".lock");
    path.with_file_name(name)
}

fn read_state(path: &Path) -> Result<FileShape, DeployError> {
    if !path.exists() {
        return Ok(FileShape::default());
    }
    let text =
        fs::read_to_string(path).map_err(|e| DeployError::fail(format!("read state: {e}")))?;
    if text.trim().is_empty() {
        return Ok(FileShape::default());
    }
    serde_json::from_str(&text).map_err(|e| DeployError::fail(format!("parse state: {e}")))
}

fn write_state(path: &Path, state: &FileShape) -> Result<(), DeployError> {
    let text = serde_json::to_string_pretty(state)
        .map_err(|e| DeployError::fail(format!("encode state: {e}")))?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| DeployError::fail(format!("write state: {e}")))?;
        file.write_all(text.as_bytes())
            .map_err(|e| DeployError::fail(format!("write state: {e}")))?;
    }
    fs::rename(&tmp, path).map_err(|e| DeployError::fail(format!("replace state: {e}")))?;
    Ok(())
}

pub fn store_path_from_env() -> PathBuf {
    if let Ok(path) = std::env::var("ECHOCHAMBER_DEPLOY_STATE") {
        return PathBuf::from(path);
    }
    directories::ProjectDirs::from("", "", "echochamber")
        .map(|d| d.config_dir().join("deployments.json"))
        .unwrap_or_else(|| PathBuf::from("deployments.json"))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::deploy::model::{DeployStatus, ProviderKind};

    #[test]
    fn round_trip_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("deployments.json"));
        let record = DeploymentRecord {
            id: "ec0123abcd01".into(),
            provider: ProviderKind::Local,
            region: "local".into(),
            zone: String::new(),
            name: "ec0123abcd".into(),
            status: DeployStatus::Ready,
            public_ip: Some("127.0.0.1".into()),
            ingest_server: Some("rtmp://127.0.0.1:1935/echochamber".into()),
            stream_key: Some("abc".into()),
            project: None,
            expires_at_unix: 10,
            created_at_unix: 1,
            error: None,
        };
        store.upsert(&record).unwrap();
        let mode = fs::metadata(store.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(
            store
                .get("ec0123abcd01")
                .unwrap()
                .unwrap()
                .stream_key
                .unwrap(),
            "abc"
        );
    }
}
