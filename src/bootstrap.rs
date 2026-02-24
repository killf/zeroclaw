use crate::config::Config;
use crate::memory::{self, Memory};
use crate::observability::{self, Observer};
use crate::providers;
use crate::runtime::{self, RuntimeAdapter};
use crate::security::SecurityPolicy;
use anyhow::Result;
use std::sync::Arc;

/// Shared runtime composition used by major entrypoints (CLI, channels, gateway).
pub struct CoreServices {
    pub observer: Arc<dyn Observer>,
    pub runtime: Arc<dyn RuntimeAdapter>,
    pub security: Arc<SecurityPolicy>,
    pub memory: Arc<dyn Memory>,
}

/// Build common runtime services from config with identical semantics across entrypoints.
pub fn build_core_services(config: &Config) -> Result<CoreServices> {
    let observer: Arc<dyn Observer> = Arc::from(observability::create_observer(&config.observability));
    let runtime: Arc<dyn RuntimeAdapter> = Arc::from(runtime::create_runtime(&config.runtime)?);
    let security = Arc::new(SecurityPolicy::from_config(
        &config.autonomy,
        &config.workspace_dir,
    ));
    let memory: Arc<dyn Memory> = Arc::from(memory::create_memory_with_storage(
        &config.memory,
        Some(&config.storage.provider.config),
        &config.workspace_dir,
        config.api_key.as_deref(),
    )?);

    Ok(CoreServices {
        observer,
        runtime,
        security,
        memory,
    })
}

/// Extract composio credentials exactly as existing entrypoints did.
pub fn composio_credentials(config: &Config) -> (Option<&str>, Option<&str>) {
    if config.composio.enabled {
        (
            config.composio.api_key.as_deref(),
            Some(config.composio.entity_id.as_str()),
        )
    } else {
        (None, None)
    }
}

/// Build provider runtime options from config.
pub fn provider_runtime_options(config: &Config) -> providers::ProviderRuntimeOptions {
    providers::ProviderRuntimeOptions {
        auth_profile_override: None,
        zeroclaw_dir: config.config_path.parent().map(std::path::PathBuf::from),
        secrets_encrypt: config.secrets.encrypt,
        reasoning_enabled: config.runtime.reasoning_enabled,
        reasoning_level: config.provider.reasoning_level.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    #[test]
    fn composio_credentials_disabled_returns_none() {
        let config = Config::default();
        let (key, entity) = composio_credentials(&config);
        assert!(key.is_none());
        assert!(entity.is_none());
    }

    #[test]
    fn composio_credentials_enabled_returns_configured_values() {
        let mut config = Config::default();
        config.composio.enabled = true;
        config.composio.api_key = Some("test-key".to_string());
        config.composio.entity_id = "entity-123".to_string();

        let (key, entity) = composio_credentials(&config);
        assert_eq!(key, Some("test-key"));
        assert_eq!(entity, Some("entity-123"));
    }

    #[test]
    fn provider_runtime_options_maps_config_fields() {
        let mut config = Config::default();
        config.config_path = PathBuf::from("/tmp/zeroclaw/config.toml");
        config.secrets.encrypt = false;
        config.runtime.reasoning_enabled = Some(true);
        let mut levels = HashMap::new();
        levels.insert("default".to_string(), "high".to_string());
        config.provider.reasoning_level = levels.clone();

        let options = provider_runtime_options(&config);
        assert_eq!(options.auth_profile_override, None);
        assert_eq!(options.zeroclaw_dir, Some(PathBuf::from("/tmp/zeroclaw")));
        assert!(!options.secrets_encrypt);
        assert_eq!(options.reasoning_enabled, Some(true));
        assert_eq!(options.reasoning_level, levels);
    }
}
