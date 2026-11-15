use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::error::DeployError;
use super::model::{is_managed, CloudInstance, ControlPlane, InstancePlan, ProviderKind, VmState};
use super::provider::CloudProvider;
use super::service::{ProviderConfig, ProviderFactory};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockEvent {
    Create(String),
    Describe(String),
    Destroy(String),
    Restrict(String),
    List,
}

#[derive(Debug, Clone)]
pub struct MockEndpoint {
    pub ip: String,
    pub rtmp_port: u16,
    pub control: ControlPlane,
}

struct Inner {
    endpoint: Option<MockEndpoint>,
    pending_left: u32,
    fail_create: bool,
    instances: BTreeMap<String, CloudInstance>,
    plans: Vec<InstancePlan>,
    events: Vec<MockEvent>,
}

/// In-memory provider. Tests point `endpoint` at local sockets; nothing here
/// talks to Hetzner or AWS.
pub struct MockProvider {
    kind: ProviderKind,
    inner: Mutex<Inner>,
}

impl MockProvider {
    pub fn new(kind: ProviderKind) -> Self {
        Self {
            kind,
            inner: Mutex::new(Inner {
                endpoint: None,
                pending_left: 0,
                fail_create: false,
                instances: BTreeMap::new(),
                plans: Vec::new(),
                events: Vec::new(),
            }),
        }
    }

    pub fn endpoint(self, endpoint: MockEndpoint) -> Self {
        self.inner.lock().expect("mock").endpoint = Some(endpoint);
        self
    }

    pub fn pending_polls(self, n: u32) -> Self {
        self.inner.lock().expect("mock").pending_left = n;
        self
    }

    pub fn fail_create(self) -> Self {
        self.inner.lock().expect("mock").fail_create = true;
        self
    }

    pub fn insert(&self, instance: CloudInstance) {
        self.inner
            .lock()
            .expect("mock")
            .instances
            .insert(instance.deployment_id.clone(), instance);
    }

    pub fn events(&self) -> Vec<MockEvent> {
        self.inner.lock().expect("mock").events.clone()
    }

    pub fn plans(&self) -> Vec<InstancePlan> {
        self.inner.lock().expect("mock").plans.clone()
    }
}

#[async_trait]
impl CloudProvider for MockProvider {
    fn kind(&self) -> ProviderKind {
        self.kind
    }

    async fn create(&self, plan: &InstancePlan) -> Result<CloudInstance, DeployError> {
        let mut inner = self.inner.lock().expect("mock");
        inner
            .events
            .push(MockEvent::Create(plan.deployment_id.clone()));
        inner.plans.push(plan.clone());
        if inner.fail_create {
            return Err(DeployError::fail("mock create failed"));
        }
        let (state, ip, port, control) = if inner.pending_left > 0 {
            (
                VmState::Pending,
                None,
                plan.ingest_port,
                ControlPlane::GuestLoopback,
            )
        } else if let Some(endpoint) = &inner.endpoint {
            (
                VmState::Running,
                Some(endpoint.ip.clone()),
                endpoint.rtmp_port,
                endpoint.control.clone(),
            )
        } else {
            (
                VmState::Pending,
                None,
                plan.ingest_port,
                ControlPlane::GuestLoopback,
            )
        };
        let inst = CloudInstance {
            deployment_id: plan.deployment_id.clone(),
            provider_id: plan.deployment_id.clone(),
            name: plan.name.clone(),
            state,
            public_ip: ip,
            rtmp_port: port,
            control,
            tags: plan.tags.clone(),
        };
        inner
            .instances
            .insert(plan.deployment_id.clone(), inst.clone());
        Ok(inst)
    }

    async fn describe(&self, deployment_id: &str) -> Result<CloudInstance, DeployError> {
        let mut inner = self.inner.lock().expect("mock");
        inner
            .events
            .push(MockEvent::Describe(deployment_id.to_string()));
        let still_pending = if inner.pending_left > 0 {
            inner.pending_left -= 1;
            true
        } else {
            false
        };
        let endpoint = inner.endpoint.clone();
        let inst = inner
            .instances
            .get_mut(deployment_id)
            .ok_or_else(|| DeployError::missing(format!("no deployment {deployment_id}")))?;
        if still_pending {
            inst.state = VmState::Pending;
            inst.public_ip = None;
        } else if let Some(endpoint) = endpoint {
            inst.state = VmState::Running;
            inst.public_ip = Some(endpoint.ip);
            inst.rtmp_port = endpoint.rtmp_port;
            inst.control = endpoint.control;
        }
        Ok(inst.clone())
    }

    async fn destroy(&self, deployment_id: &str) -> Result<(), DeployError> {
        let mut inner = self.inner.lock().expect("mock");
        inner
            .events
            .push(MockEvent::Destroy(deployment_id.to_string()));
        if let Some(inst) = inner.instances.get_mut(deployment_id) {
            inst.state = VmState::Terminated;
            inst.public_ip = None;
        }
        Ok(())
    }

    async fn list_managed(&self) -> Result<Vec<CloudInstance>, DeployError> {
        let mut inner = self.inner.lock().expect("mock");
        inner.events.push(MockEvent::List);
        Ok(inner
            .instances
            .values()
            .filter(|inst| is_managed(&inst.tags) && inst.state != VmState::Terminated)
            .cloned()
            .collect())
    }

    async fn restrict_ingress(&self, deployment_id: &str) -> Result<(), DeployError> {
        self.inner
            .lock()
            .expect("mock")
            .events
            .push(MockEvent::Restrict(deployment_id.to_string()));
        Ok(())
    }
}

pub struct FixedFactory {
    pub provider: Arc<MockProvider>,
}

#[async_trait]
impl ProviderFactory for FixedFactory {
    async fn build(&self, _cfg: &ProviderConfig) -> Result<Arc<dyn CloudProvider>, DeployError> {
        Ok(self.provider.clone())
    }
}
