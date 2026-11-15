use std::collections::BTreeMap;

use async_trait::async_trait;
use aws_sdk_lightsail::types::{NetworkProtocol, PortInfo, Tag};

use super::error::DeployError;
use super::model::{
    is_managed, validate_deployment_id, CloudInstance, ControlPlane, InstancePlan, ProviderKind,
    VmState, RTMP_PORT, TAG_ID,
};
use super::provider::CloudProvider;

pub struct LightsailProvider {
    client: aws_sdk_lightsail::Client,
    zone: String,
}

impl LightsailProvider {
    pub async fn connect(region: &str, zone: &str) -> Result<Self, DeployError> {
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_sdk_lightsail::config::Region::new(region.to_string()))
            .load()
            .await;
        Ok(Self {
            client: aws_sdk_lightsail::Client::new(&config),
            zone: zone.to_string(),
        })
    }
}

pub fn ingest_port(port: i32, cidr: &str) -> PortInfo {
    PortInfo::builder()
        .from_port(port)
        .to_port(port)
        .protocol(NetworkProtocol::Tcp)
        .cidrs(cidr)
        .build()
}

fn tags_of(map: &BTreeMap<String, String>) -> Vec<Tag> {
    map.iter()
        .map(|(key, value)| Tag::builder().key(key).value(value).build())
        .collect()
}

fn is_missing(err: &impl std::fmt::Display) -> bool {
    let text = err.to_string();
    text.contains("NotFound") || text.contains("does not exist")
}

#[async_trait]
impl CloudProvider for LightsailProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::AwsLightsail
    }

    async fn create(&self, plan: &InstancePlan) -> Result<CloudInstance, DeployError> {
        let mut tag_map = plan.tags.clone();
        tag_map.insert("echochamber-ingest-cidr".into(), plan.ingest_cidr.clone());
        let tags = tags_of(&tag_map);
        if plan.prebuilt {
            let mut request = self
                .client
                .create_instances_from_snapshot()
                .instance_names(&plan.name)
                .availability_zone(&self.zone)
                .bundle_id(&plan.size)
                .instance_snapshot_name(&plan.image)
                .user_data(&plan.user_data);
            for tag in tags {
                request = request.tags(tag);
            }
            request
                .send()
                .await
                .map_err(|e| DeployError::fail(format!("lightsail create: {e}")))?;
        } else {
            let mut request = self
                .client
                .create_instances()
                .instance_names(&plan.name)
                .availability_zone(&self.zone)
                .blueprint_id(&plan.image)
                .bundle_id(&plan.size)
                .user_data(&plan.user_data);
            for tag in tags {
                request = request.tags(tag);
            }
            request
                .send()
                .await
                .map_err(|e| DeployError::fail(format!("lightsail create: {e}")))?;
        }
        self.put_ports(&plan.name, plan.ingest_port, &plan.ingest_cidr)
            .await?;
        match self.get_named(&plan.name).await {
            Ok(inst) => Ok(inst),
            Err(_) => Ok(pending(plan)),
        }
    }

    async fn describe(&self, deployment_id: &str) -> Result<CloudInstance, DeployError> {
        validate_deployment_id(deployment_id)?;
        self.get_named(deployment_id).await
    }

    async fn destroy(&self, deployment_id: &str) -> Result<(), DeployError> {
        validate_deployment_id(deployment_id)?;
        match self
            .client
            .delete_instance()
            .instance_name(deployment_id)
            .force_delete_add_ons(true)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(err) if is_missing(&err) => Ok(()),
            Err(err) => Err(DeployError::fail(format!("lightsail delete: {err}"))),
        }
    }

    async fn list_managed(&self) -> Result<Vec<CloudInstance>, DeployError> {
        let mut page: Option<String> = None;
        let mut found = Vec::new();
        for _ in 0..20 {
            let mut request = self.client.get_instances();
            if let Some(token) = &page {
                request = request.page_token(token);
            }
            let output = request
                .send()
                .await
                .map_err(|e| DeployError::fail(format!("lightsail list: {e}")))?;
            for instance in output.instances() {
                let mapped = map_instance(instance);
                if is_managed(&mapped.tags) {
                    found.push(mapped);
                }
            }
            page = output
                .next_page_token()
                .map(|token| token.to_string())
                .filter(|token| !token.is_empty());
            if page.is_none() {
                break;
            }
        }
        Ok(found)
    }

    async fn restrict_ingress(&self, deployment_id: &str) -> Result<(), DeployError> {
        validate_deployment_id(deployment_id)?;
        let inst = self.get_named(deployment_id).await?;
        let cidr = inst
            .tags
            .get("echochamber-ingest-cidr")
            .map(String::as_str)
            .unwrap_or("0.0.0.0/0");
        self.put_ports(deployment_id, RTMP_PORT, cidr).await
    }
}

impl LightsailProvider {
    async fn put_ports(&self, name: &str, port: u16, cidr: &str) -> Result<(), DeployError> {
        let mut last = String::from("ports were not applied");
        for _ in 0..5 {
            match self
                .client
                .put_instance_public_ports()
                .instance_name(name)
                .port_infos(ingest_port(i32::from(port), cidr))
                .send()
                .await
            {
                Ok(_) => return Ok(()),
                Err(err) => {
                    last = err.to_string();
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        }
        Err(DeployError::fail(format!("lightsail ports: {last}")))
    }

    async fn get_named(&self, name: &str) -> Result<CloudInstance, DeployError> {
        let output = self
            .client
            .get_instance()
            .instance_name(name)
            .send()
            .await
            .map_err(|e| {
                if is_missing(&e) {
                    DeployError::missing(format!("no lightsail instance {name}"))
                } else {
                    DeployError::fail(format!("lightsail describe: {e}"))
                }
            })?;
        let instance = output
            .instance()
            .ok_or_else(|| DeployError::missing(format!("no lightsail instance {name}")))?;
        let mapped = map_instance(instance);
        if !is_managed(&mapped.tags) {
            return Err(DeployError::fail(format!(
                "refusing lightsail instance {name} without the managed tag"
            )));
        }
        Ok(mapped)
    }
}

fn pending(plan: &InstancePlan) -> CloudInstance {
    CloudInstance {
        deployment_id: plan.deployment_id.clone(),
        provider_id: plan.name.clone(),
        name: plan.name.clone(),
        state: VmState::Pending,
        public_ip: None,
        rtmp_port: plan.ingest_port,
        control: ControlPlane::GuestLoopback,
        tags: plan.tags.clone(),
    }
}

fn map_instance(instance: &aws_sdk_lightsail::types::Instance) -> CloudInstance {
    let name = instance.name().unwrap_or("").to_string();
    let mut tags = BTreeMap::new();
    for tag in instance.tags() {
        if let (Some(key), Some(value)) = (tag.key(), tag.value()) {
            tags.insert(key.to_string(), value.to_string());
        }
    }
    let deployment_id = tags.get(TAG_ID).cloned().unwrap_or_else(|| name.clone());
    let state_name = instance
        .state()
        .and_then(|state| state.name())
        .unwrap_or("");
    CloudInstance {
        deployment_id,
        provider_id: name.clone(),
        name,
        state: VmState::from_cloud(state_name),
        public_ip: instance
            .public_ip_address()
            .map(|ip| ip.to_string())
            .filter(|ip| !ip.is_empty()),
        rtmp_port: RTMP_PORT,
        control: ControlPlane::GuestLoopback,
        tags,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_ports_are_only_the_ingest_port() {
        let port = ingest_port(1935, "198.51.100.10/32");
        assert_eq!(port.from_port(), 1935);
        assert_eq!(port.to_port(), 1935);
        assert_eq!(port.protocol(), Some(&NetworkProtocol::Tcp));
        assert_eq!(port.cidrs(), &["198.51.100.10/32".to_string()]);
        assert!(port.cidrs().len() == 1);
    }
}
