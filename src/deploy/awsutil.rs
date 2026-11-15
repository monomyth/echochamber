use super::credentials::RequestCredentials;

/// SDK config for one request. Explicit keys replace the default chain for
/// this call only. With no keys, the AWS SDK default chain is used.
pub async fn sdk_config(region: &str, creds: &RequestCredentials) -> aws_config::SdkConfig {
    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_sdk_lightsail::config::Region::new(region.to_string()));
    if let Some(keys) = &creds.aws {
        let provider = aws_sdk_lightsail::config::Credentials::new(
            keys.access_key_id.clone(),
            keys.secret_access_key.clone(),
            keys.session_token.clone(),
            None,
            "echochamber-request",
        );
        loader = loader.credentials_provider(provider);
    }
    loader.load().await
}
