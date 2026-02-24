use crate::config::{runtime_plugins_config, PluginDefinition, PluginKind, PluginsConfig};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct PluginRegistry {
    plugins: HashMap<String, ResolvedPlugin>,
}

#[derive(Debug, Clone)]
pub struct ResolvedPlugin {
    pub id: String,
    pub kind: PluginKind,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub timeout_secs: u64,
    pub source: PluginSource,
}

#[derive(Debug, Clone)]
pub enum PluginSource {
    Config,
    Manifest(PathBuf),
}

#[derive(Debug, Clone, Deserialize)]
struct PluginManifest {
    #[serde(default)]
    id: Option<String>,
    #[serde(default = "default_manifest_enabled")]
    enabled: bool,
    kind: PluginKind,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default = "default_manifest_timeout_secs")]
    timeout_secs: u64,
}

fn default_manifest_enabled() -> bool {
    true
}

fn default_manifest_timeout_secs() -> u64 {
    30
}

impl PluginRegistry {
    pub fn from_runtime(workspace_dir: &Path) -> Result<Self> {
        let config = runtime_plugins_config();
        Self::from_config(&config, workspace_dir)
    }

    pub fn from_config(config: &PluginsConfig, workspace_dir: &Path) -> Result<Self> {
        if !config.enabled {
            return Ok(Self::default());
        }

        let mut plugins = HashMap::new();

        if let Some(directory) = config
            .directory
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            for resolved in load_manifest_plugins(directory, workspace_dir)? {
                let key = normalize_plugin_id(&resolved.id);
                plugins.insert(key, resolved);
            }
        }

        for (plugin_id, plugin) in &config.registry {
            if !plugin.enabled {
                continue;
            }

            validate_plugin_shape(plugin_id, plugin)
                .with_context(|| format!("Invalid plugins.registry.{plugin_id} definition"))?;

            let resolved = ResolvedPlugin {
                id: plugin_id.to_string(),
                kind: plugin.kind,
                command: plugin.command.trim().to_string(),
                args: plugin.args.clone(),
                env: plugin.env.clone(),
                timeout_secs: plugin.timeout_secs,
                source: PluginSource::Config,
            };
            plugins.insert(normalize_plugin_id(plugin_id), resolved);
        }

        Ok(Self { plugins })
    }

    pub fn get(&self, plugin_id: &str) -> Option<&ResolvedPlugin> {
        self.plugins.get(&normalize_plugin_id(plugin_id))
    }

    pub fn memory(&self, plugin_id: &str) -> Option<&ResolvedPlugin> {
        self.get(plugin_id)
            .filter(|plugin| plugin.kind == PluginKind::Memory)
    }

    pub fn ids_by_kind(&self, kind: PluginKind) -> Vec<String> {
        let mut ids: Vec<String> = self
            .plugins
            .values()
            .filter(|plugin| plugin.kind == kind)
            .map(|plugin| plugin.id.clone())
            .collect();
        ids.sort();
        ids
    }

    pub fn total(&self) -> usize {
        self.plugins.len()
    }
}

fn resolve_plugin_dir(directory: &str, workspace_dir: &Path) -> PathBuf {
    let path = PathBuf::from(directory);
    if path.is_absolute() {
        path
    } else {
        workspace_dir.join(path)
    }
}

fn load_manifest_plugins(directory: &str, workspace_dir: &Path) -> Result<Vec<ResolvedPlugin>> {
    let dir = resolve_plugin_dir(directory, workspace_dir);
    if !dir.exists() {
        tracing::warn!(path = %dir.display(), "Plugin directory does not exist; skipping manifest discovery");
        return Ok(Vec::new());
    }

    let mut loaded = Vec::new();

    for entry in fs::read_dir(&dir)
        .with_context(|| format!("Failed to read plugin directory '{}':", dir.display()))?
    {
        let entry =
            entry.with_context(|| format!("Failed to read plugin entry in {}", dir.display()))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
            continue;
        };

        let Some(manifest) = parse_manifest(&path, ext) else {
            continue;
        };

        let manifest = manifest
            .with_context(|| format!("Failed to parse plugin manifest '{}':", path.display()))?;
        if !manifest.enabled {
            continue;
        }

        let plugin_id = manifest
            .id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string()
            });

        validate_manifest_shape(&plugin_id, &manifest)
            .with_context(|| format!("Invalid plugin manifest '{}':", path.display()))?;

        loaded.push(ResolvedPlugin {
            id: plugin_id,
            kind: manifest.kind,
            command: manifest.command.trim().to_string(),
            args: manifest.args,
            env: manifest.env,
            timeout_secs: manifest.timeout_secs,
            source: PluginSource::Manifest(path),
        });
    }

    Ok(loaded)
}

fn parse_manifest(path: &Path, extension: &str) -> Option<Result<PluginManifest>> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) => {
            return Some(
                Err(error).with_context(|| format!("Unable to read file '{}':", path.display())),
            );
        }
    };

    match extension.to_ascii_lowercase().as_str() {
        "toml" => Some(
            toml::from_str::<PluginManifest>(&content)
                .with_context(|| format!("Invalid TOML in '{}':", path.display())),
        ),
        "json" => Some(
            serde_json::from_str::<PluginManifest>(&content)
                .with_context(|| format!("Invalid JSON in '{}':", path.display())),
        ),
        _ => None,
    }
}

fn normalize_plugin_id(plugin_id: &str) -> String {
    plugin_id.trim().to_ascii_lowercase()
}

fn is_valid_plugin_id(plugin_id: &str) -> bool {
    !plugin_id.trim().is_empty()
        && plugin_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

fn validate_manifest_shape(plugin_id: &str, manifest: &PluginManifest) -> Result<()> {
    let proxy = PluginDefinition {
        enabled: manifest.enabled,
        kind: manifest.kind,
        command: manifest.command.clone(),
        args: manifest.args.clone(),
        env: manifest.env.clone(),
        timeout_secs: manifest.timeout_secs,
    };
    validate_plugin_shape(plugin_id, &proxy)
}

fn validate_plugin_shape(plugin_id: &str, plugin: &PluginDefinition) -> Result<()> {
    if !is_valid_plugin_id(plugin_id) {
        anyhow::bail!(
            "invalid plugin ID '{}'; use only letters, digits, '-' or '_'",
            plugin_id
        );
    }

    if plugin.command.trim().is_empty() {
        anyhow::bail!("plugin command must not be empty");
    }

    for (i, arg) in plugin.args.iter().enumerate() {
        if arg.trim().is_empty() {
            anyhow::bail!("plugin args[{i}] must not be empty");
        }
    }

    if plugin.timeout_secs == 0 {
        anyhow::bail!("plugin timeout_secs must be greater than 0");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_plugins_return_empty_registry() {
        let cfg = PluginsConfig::default();
        let reg = PluginRegistry::from_config(&cfg, std::path::Path::new(".")).unwrap();
        assert_eq!(reg.total(), 0);
    }

    #[test]
    fn registry_plugins_are_loaded_and_sorted_by_kind() {
        let mut cfg = PluginsConfig {
            enabled: true,
            directory: None,
            registry: HashMap::new(),
        };
        cfg.registry.insert(
            "mem_alpha".into(),
            PluginDefinition {
                enabled: true,
                kind: PluginKind::Memory,
                command: "echo".into(),
                args: vec!["ok".into()],
                env: HashMap::new(),
                timeout_secs: 10,
            },
        );
        cfg.registry.insert(
            "sec_guard".into(),
            PluginDefinition {
                enabled: true,
                kind: PluginKind::Security,
                command: "echo".into(),
                args: vec!["ok".into()],
                env: HashMap::new(),
                timeout_secs: 10,
            },
        );

        let reg = PluginRegistry::from_config(&cfg, std::path::Path::new(".")).unwrap();
        assert_eq!(reg.total(), 2);
        assert_eq!(reg.ids_by_kind(PluginKind::Memory), vec!["mem_alpha"]);
        assert_eq!(reg.ids_by_kind(PluginKind::Security), vec!["sec_guard"]);
    }
}
