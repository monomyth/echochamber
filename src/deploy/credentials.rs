use std::fmt;

use super::error::DeployError;
use super::model::{DeployRequest, ProviderKind, ReapRequest};

/// Keys the device already has. Echochamber does not write these down.
#[derive(Clone)]
pub struct AwsKeys {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

impl fmt::Debug for AwsKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AwsKeys")
            .field("access_key_id", &"set")
            .field("secret_access_key", &"set")
            .field("session_token", &self.session_token.as_ref().map(|_| "set"))
            .finish()
    }
}

/// A parsed service-account key. The JSON is kept only to hand to the GCP auth library.
#[derive(Clone)]
pub struct GcpServiceAccount {
    pub project_id: String,
    pub client_email: String,
    json: String,
}

impl GcpServiceAccount {
    pub fn json(&self) -> &str {
        &self.json
    }
}

impl fmt::Debug for GcpServiceAccount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GcpServiceAccount")
            .field("project_id", &self.project_id)
            .field("client_email", &self.client_email)
            .field("private_key", &"set")
            .finish()
    }
}

#[derive(Clone, Default)]
pub struct RequestCredentials {
    pub aws: Option<AwsKeys>,
    pub gcp: Option<GcpServiceAccount>,
}

impl fmt::Debug for RequestCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestCredentials")
            .field("aws", &self.aws)
            .field("gcp", &self.gcp)
            .finish()
    }
}

#[derive(Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialFields {
    #[serde(default)]
    pub aws_access_key_id: Option<String>,
    #[serde(default)]
    pub aws_secret_access_key: Option<String>,
    #[serde(default)]
    pub aws_session_token: Option<String>,
    #[serde(default)]
    pub gcp_service_account_json: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
}

impl fmt::Debug for CredentialFields {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialFields")
            .field("aws_access_key_id", &presence(&self.aws_access_key_id))
            .field(
                "aws_secret_access_key",
                &presence(&self.aws_secret_access_key),
            )
            .field("aws_session_token", &presence(&self.aws_session_token))
            .field(
                "gcp_service_account_json",
                &presence(&self.gcp_service_account_json),
            )
            .field("project", &self.project)
            .finish()
    }
}

fn presence(value: &Option<String>) -> &'static str {
    if value.as_ref().is_some_and(|s| !s.is_empty()) {
        "set"
    } else {
        "unset"
    }
}

pub fn parse_gcp_service_account(json: &str) -> Result<GcpServiceAccount, DeployError> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|_| DeployError::bad("gcp_service_account_json is not JSON"))?;
    let type_ok = value.get("type").and_then(|v| v.as_str()) == Some("service_account");
    let project_id = value
        .get("project_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let client_email = value
        .get("client_email")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let private_key = value
        .get("private_key")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !type_ok
        || project_id.is_empty()
        || client_email.is_empty()
        || !private_key.contains("PRIVATE KEY")
    {
        return Err(DeployError::bad(
            "gcp_service_account_json must be a service account key with project_id, client_email, and private_key",
        ));
    }
    Ok(GcpServiceAccount {
        project_id: project_id.to_string(),
        client_email: client_email.to_string(),
        json: json.to_string(),
    })
}

pub fn credentials_for(
    kind: ProviderKind,
    access_key: Option<&str>,
    secret: Option<&str>,
    session: Option<&str>,
    gcp_json: Option<&str>,
) -> Result<RequestCredentials, DeployError> {
    let access_key = blank_as_none(access_key);
    let secret = blank_as_none(secret);
    let session = blank_as_none(session);
    let gcp_json = blank_as_none(gcp_json);
    let aws = match (access_key, secret, session) {
        (None, None, None) => None,
        (Some(id), Some(secret), session) => Some(AwsKeys {
            access_key_id: id.to_string(),
            secret_access_key: secret.to_string(),
            session_token: session.map(str::to_string),
        }),
        _ => {
            return Err(DeployError::bad(
                "aws_access_key_id and aws_secret_access_key must be sent together",
            ))
        }
    };
    let gcp = gcp_json.map(parse_gcp_service_account).transpose()?;
    match kind {
        ProviderKind::AwsEc2 | ProviderKind::AwsLightsail => {
            if gcp.is_some() {
                return Err(DeployError::bad(
                    "gcp_service_account_json is only used with --provider gcp",
                ));
            }
        }
        ProviderKind::Gcp => {
            if aws.is_some() {
                return Err(DeployError::bad(
                    "AWS keys are only used with --provider aws-ec2 or aws-lightsail",
                ));
            }
        }
        ProviderKind::Hetzner | ProviderKind::Local => {
            if aws.is_some() || gcp.is_some() {
                return Err(DeployError::bad(
                    "this provider does not take AWS or GCP credentials on the request",
                ));
            }
        }
    }
    Ok(RequestCredentials { aws, gcp })
}

fn blank_as_none(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
}

impl DeployRequest {
    pub fn credentials(&self) -> Result<RequestCredentials, DeployError> {
        credentials_for(
            self.provider,
            self.aws_access_key_id.as_deref(),
            self.aws_secret_access_key.as_deref(),
            self.aws_session_token.as_deref(),
            self.gcp_service_account_json.as_deref(),
        )
    }
}

impl ReapRequest {
    pub fn credentials(&self) -> Result<RequestCredentials, DeployError> {
        credentials_for(
            self.provider,
            self.aws_access_key_id.as_deref(),
            self.aws_secret_access_key.as_deref(),
            self.aws_session_token.as_deref(),
            self.gcp_service_account_json.as_deref(),
        )
    }
}

impl CredentialFields {
    pub fn credentials_for(&self, kind: ProviderKind) -> Result<RequestCredentials, DeployError> {
        credentials_for(
            kind,
            self.aws_access_key_id.as_deref(),
            self.aws_secret_access_key.as_deref(),
            self.aws_session_token.as_deref(),
            self.gcp_service_account_json.as_deref(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_does_not_contain_the_secret() {
        let fields = CredentialFields {
            aws_access_key_id: Some("AKIATESTEXAMPLE".into()),
            aws_secret_access_key: Some("super-secret-value".into()),
            aws_session_token: Some("session-token-value".into()),
            gcp_service_account_json: None,
            project: None,
        };
        let rendered = format!("{fields:?}");
        assert!(!rendered.contains("super-secret-value"));
        assert!(!rendered.contains("session-token-value"));
        assert!(!rendered.contains("AKIATESTEXAMPLE"));
        let creds = credentials_for(
            ProviderKind::AwsEc2,
            fields.aws_access_key_id.as_deref(),
            fields.aws_secret_access_key.as_deref(),
            fields.aws_session_token.as_deref(),
            None,
        )
        .unwrap();
        let rendered = format!("{creds:?}");
        assert!(!rendered.contains("super-secret-value"));
        assert!(!rendered.contains("session-token-value"));
    }

    #[test]
    fn a_secret_without_an_access_key_is_rejected() {
        let err = credentials_for(ProviderKind::AwsEc2, None, Some("secret"), None, None);
        assert!(err.is_err());
    }

    #[test]
    fn gcp_json_must_look_like_a_service_account() {
        let err = parse_gcp_service_account(r#"{"type":"service_account","project_id":"demo"}"#);
        assert!(err.is_err());
        let ok = parse_gcp_service_account(
            r#"{"type":"service_account","project_id":"demo-project","client_email":"relay@demo-project.iam.gserviceaccount.com","private_key":"-----BEGIN PRIVATE KEY-----\nMII\n-----END PRIVATE KEY-----\n"}"#,
        )
        .unwrap();
        assert_eq!(ok.project_id, "demo-project");
        let rendered = format!("{ok:?}");
        assert!(!rendered.contains("BEGIN PRIVATE KEY"));
    }
}
