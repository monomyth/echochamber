use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use aws_sdk_ec2::types::{
    Filter, HttpTokensState, InstanceMetadataEndpointState, InstanceMetadataOptionsRequest,
    InstanceNetworkInterfaceSpecification, InstanceType, IpPermission, IpRange, ResourceType,
    ShutdownBehavior, Tag, TagSpecification,
};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;

use super::awsutil;
use super::credentials::RequestCredentials;
use super::error::DeployError;
use super::model::{
    is_managed, validate_deployment_id, CloudInstance, ControlPlane, InstancePlan, ProviderKind,
    VmState, RTMP_PORT, TAG_ID, TAG_MANAGED, TAG_SECURITY_GROUP,
};
use super::provider::CloudProvider;

/// What an EC2 launch is allowed to open and how the guest's shutdown ends.
#[derive(Debug, Clone)]
pub struct Ec2Launch {
    pub deployment_id: String,
    pub name: String,
    pub image_id: String,
    pub instance_type: String,
    pub user_data: String,
    pub ingest_port: u16,
    pub ingest_cidr: String,
    pub tags: BTreeMap<String, String>,
    /// Guest `shutdown` deletes the instance. Lightsail cannot do this.
    pub shutdown_behavior: &'static str,
    pub imds_tokens: &'static str,
}

pub fn plan_launch(plan: &InstancePlan) -> Result<Ec2Launch, DeployError> {
    if plan.user_data.len() > 16_000 {
        return Err(DeployError::bad(
            "EC2 user data is over 16KB. Use a smaller config or a prebuilt AMI.",
        ));
    }
    if !plan.image.starts_with("ami-") {
        return Err(DeployError::bad(
            "EC2 --image must be an AMI id starting with ami-",
        ));
    }
    Ok(Ec2Launch {
        deployment_id: plan.deployment_id.clone(),
        name: plan.name.clone(),
        image_id: plan.image.clone(),
        instance_type: plan.size.clone(),
        user_data: plan.user_data.clone(),
        ingest_port: plan.ingest_port,
        ingest_cidr: plan.ingest_cidr.clone(),
        tags: plan.tags.clone(),
        shutdown_behavior: "terminate",
        imds_tokens: "required",
    })
}

#[async_trait]
pub trait Ec2Api: Send + Sync {
    async fn launch(&self, spec: &Ec2Launch, zone: &str) -> Result<CloudInstance, DeployError>;
    async fn describe(&self, deployment_id: &str) -> Result<CloudInstance, DeployError>;
    async fn destroy(&self, deployment_id: &str) -> Result<(), DeployError>;
    async fn list_managed(&self) -> Result<Vec<CloudInstance>, DeployError>;
}

pub struct Ec2Provider {
    api: Arc<dyn Ec2Api>,
}

impl Ec2Provider {
    pub async fn connect(
        region: &str,
        zone: &str,
        creds: &RequestCredentials,
    ) -> Result<Self, DeployError> {
        let config = awsutil::sdk_config(region, creds).await;
        let client = aws_sdk_ec2::Client::new(&config);
        Ok(Self {
            api: Arc::new(SdkEc2 {
                client,
                zone: zone.to_string(),
            }),
        })
    }

    #[cfg(test)]
    pub fn from_api(api: Arc<dyn Ec2Api>) -> Self {
        Self { api }
    }
}

#[async_trait]
impl CloudProvider for Ec2Provider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::AwsEc2
    }

    async fn create(&self, plan: &InstancePlan) -> Result<CloudInstance, DeployError> {
        let spec = plan_launch(plan)?;
        self.api.launch(&spec, &plan.zone).await
    }

    async fn describe(&self, deployment_id: &str) -> Result<CloudInstance, DeployError> {
        validate_deployment_id(deployment_id)?;
        self.api.describe(deployment_id).await
    }

    async fn destroy(&self, deployment_id: &str) -> Result<(), DeployError> {
        validate_deployment_id(deployment_id)?;
        self.api.destroy(deployment_id).await
    }

    async fn list_managed(&self) -> Result<Vec<CloudInstance>, DeployError> {
        self.api.list_managed().await
    }
}

struct SdkEc2 {
    client: aws_sdk_ec2::Client,
    zone: String,
}

#[async_trait]
impl Ec2Api for SdkEc2 {
    async fn launch(&self, spec: &Ec2Launch, zone: &str) -> Result<CloudInstance, DeployError> {
        let zone = if zone.is_empty() { &self.zone } else { zone };
        let (subnet, vpc) = self.default_subnet(zone).await?;
        let group_id = self.create_group(spec, &vpc).await?;
        if let Err(err) = self.allow_ingest(&group_id, spec).await {
            let _ = self.delete_group(&group_id).await;
            return Err(err);
        }
        let mut tags = spec.tags.clone();
        tags.insert(TAG_SECURITY_GROUP.to_string(), group_id.clone());
        match self.run(spec, &subnet, &group_id, &tags).await {
            Ok(id) => self
                .describe(&spec.deployment_id)
                .await
                .or(Ok(pending(spec, &id, &tags))),
            Err(err) => {
                let _ = self.delete_group(&group_id).await;
                Err(err)
            }
        }
    }

    async fn describe(&self, deployment_id: &str) -> Result<CloudInstance, DeployError> {
        let output = self
            .client
            .describe_instances()
            .filters(tag_filter(TAG_ID, deployment_id))
            .send()
            .await
            .map_err(|e| DeployError::fail(format!("ec2 describe: {e}")))?;
        for reservation in output.reservations() {
            for instance in reservation.instances() {
                let mapped = map_instance(instance);
                if is_managed(&mapped.tags) && mapped.deployment_id == deployment_id {
                    return Ok(mapped);
                }
            }
        }
        Err(DeployError::missing(format!(
            "no ec2 instance {deployment_id}"
        )))
    }

    async fn destroy(&self, deployment_id: &str) -> Result<(), DeployError> {
        let output = self
            .client
            .describe_instances()
            .filters(tag_filter(TAG_ID, deployment_id))
            .send()
            .await
            .map_err(|e| DeployError::fail(format!("ec2 describe: {e}")))?;
        for reservation in output.reservations() {
            for instance in reservation.instances() {
                if let Some(id) = instance.instance_id() {
                    let result = self
                        .client
                        .terminate_instances()
                        .instance_ids(id)
                        .send()
                        .await;
                    if let Err(err) = result {
                        if !is_missing(&err) {
                            return Err(DeployError::fail(format!("ec2 terminate: {err}")));
                        }
                    }
                }
            }
        }
        let groups = self
            .client
            .describe_security_groups()
            .filters(tag_filter(TAG_ID, deployment_id))
            .send()
            .await
            .map_err(|e| DeployError::fail(format!("ec2 security groups: {e}")))?;
        for group in groups.security_groups() {
            if let Some(id) = group.group_id() {
                self.delete_group(id).await?;
            }
        }
        Ok(())
    }

    async fn list_managed(&self) -> Result<Vec<CloudInstance>, DeployError> {
        let mut found = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut token: Option<String> = None;
        loop {
            let mut request = self
                .client
                .describe_instances()
                .filters(tag_filter(TAG_MANAGED, "true"));
            if let Some(next) = &token {
                request = request.next_token(next);
            }
            let output = request
                .send()
                .await
                .map_err(|e| DeployError::fail(format!("ec2 list: {e}")))?;
            for reservation in output.reservations() {
                for instance in reservation.instances() {
                    let mapped = map_instance(instance);
                    if is_managed(&mapped.tags) && mapped.state != VmState::Terminated {
                        seen.insert(mapped.deployment_id.clone());
                        found.push(mapped);
                    }
                }
            }
            token = output
                .next_token()
                .map(str::to_string)
                .filter(|s| !s.is_empty());
            if token.is_none() {
                break;
            }
        }
        let groups = self
            .client
            .describe_security_groups()
            .filters(tag_filter(TAG_MANAGED, "true"))
            .send()
            .await
            .map_err(|e| DeployError::fail(format!("ec2 security groups: {e}")))?;
        for group in groups.security_groups() {
            let tags = tag_map(group.tags());
            let Some(id) = tags.get(TAG_ID).cloned() else {
                continue;
            };
            if seen.contains(&id) || !is_managed(&tags) {
                continue;
            }
            found.push(CloudInstance {
                deployment_id: id,
                provider_id: group.group_id().unwrap_or("").to_string(),
                name: group.group_name().unwrap_or("").to_string(),
                state: VmState::Terminated,
                public_ip: None,
                rtmp_port: RTMP_PORT,
                control: ControlPlane::GuestLoopback,
                tags,
            });
        }
        Ok(found)
    }
}

impl SdkEc2 {
    async fn default_subnet(&self, zone: &str) -> Result<(String, String), DeployError> {
        let output = self
            .client
            .describe_subnets()
            .filters(
                Filter::builder()
                    .name("availability-zone")
                    .values(zone)
                    .build(),
            )
            .filters(
                Filter::builder()
                    .name("default-for-az")
                    .values("true")
                    .build(),
            )
            .send()
            .await
            .map_err(|e| DeployError::fail(format!("ec2 subnets: {e}")))?;
        let subnet = output.subnets().first().ok_or_else(|| {
            DeployError::fail(format!(
                "no default subnet in {zone}. Pass --zone for an availability zone that has one."
            ))
        })?;
        let id = subnet
            .subnet_id()
            .ok_or_else(|| DeployError::fail("subnet has no id"))?;
        let vpc = subnet
            .vpc_id()
            .ok_or_else(|| DeployError::fail("subnet has no vpc"))?;
        Ok((id.to_string(), vpc.to_string()))
    }

    async fn create_group(&self, spec: &Ec2Launch, vpc: &str) -> Result<String, DeployError> {
        let output = self
            .client
            .create_security_group()
            .group_name(&spec.name)
            .description("echochamber ephemeral relay")
            .vpc_id(vpc)
            .tag_specifications(tag_spec(ResourceType::SecurityGroup, &spec.tags))
            .send()
            .await
            .map_err(|e| DeployError::fail(format!("ec2 security group: {e}")))?;
        output
            .group_id()
            .map(str::to_string)
            .ok_or_else(|| DeployError::fail("ec2 security group response had no id"))
    }

    async fn allow_ingest(&self, group_id: &str, spec: &Ec2Launch) -> Result<(), DeployError> {
        self.client
            .authorize_security_group_ingress()
            .group_id(group_id)
            .ip_permissions(
                IpPermission::builder()
                    .ip_protocol("tcp")
                    .from_port(i32::from(spec.ingest_port))
                    .to_port(i32::from(spec.ingest_port))
                    .ip_ranges(IpRange::builder().cidr_ip(&spec.ingest_cidr).build())
                    .build(),
            )
            .send()
            .await
            .map_err(|e| DeployError::fail(format!("ec2 ingress: {e}")))?;
        Ok(())
    }

    async fn run(
        &self,
        spec: &Ec2Launch,
        subnet: &str,
        group_id: &str,
        tags: &BTreeMap<String, String>,
    ) -> Result<String, DeployError> {
        let shutdown = match spec.shutdown_behavior {
            "terminate" => ShutdownBehavior::Terminate,
            other => {
                return Err(DeployError::fail(format!(
                    "refusing ec2 shutdown behavior {other}"
                )))
            }
        };
        let tokens = match spec.imds_tokens {
            "required" => HttpTokensState::Required,
            other => {
                return Err(DeployError::fail(format!(
                    "refusing ec2 metadata tokens {other}"
                )))
            }
        };
        let user_data = STANDARD.encode(spec.user_data.as_bytes());
        let output = self
            .client
            .run_instances()
            .image_id(&spec.image_id)
            .instance_type(InstanceType::from(spec.instance_type.as_str()))
            .min_count(1)
            .max_count(1)
            .user_data(user_data)
            .instance_initiated_shutdown_behavior(shutdown)
            .metadata_options(
                InstanceMetadataOptionsRequest::builder()
                    .http_tokens(tokens)
                    .http_endpoint(InstanceMetadataEndpointState::Enabled)
                    .build(),
            )
            .network_interfaces(
                InstanceNetworkInterfaceSpecification::builder()
                    .device_index(0)
                    .associate_public_ip_address(true)
                    .subnet_id(subnet)
                    .groups(group_id)
                    .build(),
            )
            .tag_specifications(tag_spec(ResourceType::Instance, tags))
            .tag_specifications(tag_spec(ResourceType::Volume, tags))
            .send()
            .await
            .map_err(|e| DeployError::fail(format!("ec2 run: {e}")))?;
        output
            .instances()
            .first()
            .and_then(|instance| instance.instance_id())
            .map(str::to_string)
            .ok_or_else(|| DeployError::fail("ec2 run response had no instance id"))
    }

    async fn delete_group(&self, group_id: &str) -> Result<(), DeployError> {
        let mut last = String::from("security group was not deleted");
        for _ in 0..5 {
            match self
                .client
                .delete_security_group()
                .group_id(group_id)
                .send()
                .await
            {
                Ok(_) => return Ok(()),
                Err(err) if is_missing(&err) => return Ok(()),
                Err(err) => {
                    last = err.to_string();
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        }
        Err(DeployError::fail(format!(
            "ec2 delete security group: {last}"
        )))
    }
}

fn pending(spec: &Ec2Launch, instance_id: &str, tags: &BTreeMap<String, String>) -> CloudInstance {
    CloudInstance {
        deployment_id: spec.deployment_id.clone(),
        provider_id: instance_id.to_string(),
        name: spec.name.clone(),
        state: VmState::Pending,
        public_ip: None,
        rtmp_port: spec.ingest_port,
        control: ControlPlane::GuestLoopback,
        tags: tags.clone(),
    }
}

fn tag_spec(kind: ResourceType, tags: &BTreeMap<String, String>) -> TagSpecification {
    let mut spec = TagSpecification::builder().resource_type(kind);
    for (key, value) in tags {
        spec = spec.tags(Tag::builder().key(key).value(value).build());
    }
    spec.build()
}

fn tag_filter(key: &str, value: &str) -> Filter {
    let name = if key.starts_with("instance-") || key == "availability-zone" {
        key.to_string()
    } else {
        format!("tag:{key}")
    };
    Filter::builder().name(name).values(value).build()
}

fn tag_map(tags: &[Tag]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for tag in tags {
        if let (Some(key), Some(value)) = (tag.key(), tag.value()) {
            out.insert(key.to_string(), value.to_string());
        }
    }
    out
}

fn map_instance(instance: &aws_sdk_ec2::types::Instance) -> CloudInstance {
    let mut tags = tag_map(instance.tags());
    if let Some(group) = instance
        .security_groups()
        .first()
        .and_then(|g| g.group_id())
    {
        tags.entry(TAG_SECURITY_GROUP.to_string())
            .or_insert_with(|| group.to_string());
    }
    let name = instance.instance_id().unwrap_or("").to_string();
    let deployment_id = tags.get(TAG_ID).cloned().unwrap_or_else(|| name.clone());
    let state_name = instance
        .state()
        .and_then(|state| state.name())
        .map(|name| name.as_str())
        .unwrap_or("");
    CloudInstance {
        deployment_id,
        provider_id: name.clone(),
        name,
        state: VmState::from_cloud(state_name),
        public_ip: instance
            .public_ip_address()
            .map(str::to_string)
            .filter(|ip| !ip.is_empty()),
        rtmp_port: RTMP_PORT,
        control: ControlPlane::GuestLoopback,
        tags,
    }
}

fn is_missing(err: &impl std::fmt::Display) -> bool {
    let text = err.to_string();
    text.contains("NotFound") || text.contains(".NotFound") || text.contains("does not exist")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::QualityConfig;
    use std::sync::Mutex;

    fn sample_plan() -> InstancePlan {
        InstancePlan {
            deployment_id: "ec0123abcd01".into(),
            name: "ec0123abcd01".into(),
            region: "us-east-1".into(),
            zone: "us-east-1a".into(),
            image: "ami-0123456789abcdef0".into(),
            prebuilt: true,
            size: "t3.small".into(),
            ingest_cidr: "203.0.113.9/32".into(),
            ingest_port: 1935,
            user_data: "#!/bin/bash\necho hi\n".into(),
            tags: super::super::model::managed_tags("ec0123abcd01", 1, 99),
            expires_at_unix: 99,
            created_at_unix: 1,
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
    fn launch_terminates_on_guest_shutdown_and_opens_only_ingest() {
        let spec = plan_launch(&sample_plan()).unwrap();
        assert_eq!(spec.shutdown_behavior, "terminate");
        assert_eq!(spec.imds_tokens, "required");
        assert_eq!(spec.ingest_port, 1935);
        assert_eq!(spec.ingest_cidr, "203.0.113.9/32");
        assert_eq!(spec.tags.get(TAG_MANAGED).map(String::as_str), Some("true"));
        assert!(spec.user_data.contains("#!/bin/bash"));
    }

    struct FakeEc2 {
        launches: Mutex<Vec<Ec2Launch>>,
        alive: Mutex<BTreeMap<String, CloudInstance>>,
    }

    #[async_trait]
    impl Ec2Api for FakeEc2 {
        async fn launch(
            &self,
            spec: &Ec2Launch,
            _zone: &str,
        ) -> Result<CloudInstance, DeployError> {
            assert_eq!(spec.shutdown_behavior, "terminate");
            assert_eq!(spec.ingest_port, 1935);
            self.launches.lock().expect("launches").push(spec.clone());
            let inst = CloudInstance {
                deployment_id: spec.deployment_id.clone(),
                provider_id: "i-abc".into(),
                name: spec.name.clone(),
                state: VmState::Running,
                public_ip: Some("203.0.113.20".into()),
                rtmp_port: spec.ingest_port,
                control: ControlPlane::GuestLoopback,
                tags: spec.tags.clone(),
            };
            self.alive
                .lock()
                .expect("alive")
                .insert(spec.deployment_id.clone(), inst.clone());
            Ok(inst)
        }

        async fn describe(&self, deployment_id: &str) -> Result<CloudInstance, DeployError> {
            self.alive
                .lock()
                .expect("alive")
                .get(deployment_id)
                .cloned()
                .ok_or_else(|| DeployError::missing(format!("no ec2 instance {deployment_id}")))
        }

        async fn destroy(&self, deployment_id: &str) -> Result<(), DeployError> {
            self.alive.lock().expect("alive").remove(deployment_id);
            Ok(())
        }

        async fn list_managed(&self) -> Result<Vec<CloudInstance>, DeployError> {
            Ok(self
                .alive
                .lock()
                .expect("alive")
                .values()
                .filter(|inst| is_managed(&inst.tags))
                .cloned()
                .collect())
        }
    }

    #[tokio::test]
    async fn provider_uses_the_launch_spec_and_can_destroy() {
        let fake = Arc::new(FakeEc2 {
            launches: Mutex::new(Vec::new()),
            alive: Mutex::new(BTreeMap::new()),
        });
        let provider = Ec2Provider::from_api(fake.clone());
        let created = provider.create(&sample_plan()).await.unwrap();
        assert_eq!(created.public_ip.as_deref(), Some("203.0.113.20"));
        assert!(matches!(created.control, ControlPlane::GuestLoopback));
        provider.destroy("ec0123abcd01").await.unwrap();
        assert!(provider.describe("ec0123abcd01").await.is_err());
        assert_eq!(fake.launches.lock().unwrap().len(), 1);
    }
}
