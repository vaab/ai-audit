use serde::Deserialize;

#[derive(Debug, Deserialize, Default, Clone)]
pub struct OpencodeServerConfig {
    pub url: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct OpencodeConfig {
    #[serde(default)]
    pub server: OpencodeServerConfig,
}

impl OpencodeConfig {
    pub fn from_generic(config: &crate::config::Config) -> Self {
        config
            .provider_block("opencode-server")
            .and_then(|raw| serde_yaml::from_value::<OpencodeServerConfig>(raw.clone()).ok())
            .map(|server| Self { server })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    #[test]
    fn reads_flat_opencode_server_block() {
        let config: crate::config::Config = serde_yaml::from_str(indoc! {"
            opencode-server:
              url: http://127.0.0.1:4096
              password: secret
        "})
        .unwrap();

        let parsed = OpencodeConfig::from_generic(&config);
        assert_eq!(parsed.server.url.as_deref(), Some("http://127.0.0.1:4096"));
        assert_eq!(parsed.server.password.as_deref(), Some("secret"));
    }

    #[test]
    fn missing_block_defaults() {
        let config = crate::config::Config::default();
        let parsed = OpencodeConfig::from_generic(&config);
        assert!(parsed.server.url.is_none());
        assert!(parsed.server.password.is_none());
    }
}
