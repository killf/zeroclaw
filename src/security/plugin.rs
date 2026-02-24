use super::policy::{AutonomyLevel, SecurityPolicy};
use crate::config::PluginKind;
use crate::plugins::{PluginRegistry, ResolvedPlugin};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Serialize)]
struct SecurityPluginRequest {
    protocol: &'static str,
    subsystem: &'static str,
    plugin: String,
    operation: &'static str,
    payload: SecurityPolicySnapshot,
}

#[derive(Debug, Serialize)]
struct SecurityPolicySnapshot {
    autonomy: AutonomyLevel,
    workspace_only: bool,
    allowed_commands: Vec<String>,
    forbidden_paths: Vec<String>,
    allowed_roots: Vec<String>,
    max_actions_per_hour: u32,
    max_cost_per_day_cents: u32,
    require_approval_for_medium_risk: bool,
    block_high_risk_commands: bool,
    shell_env_passthrough: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SecurityPluginResponse<T> {
    #[serde(default)]
    ok: bool,
    data: Option<T>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct SecurityPolicyOverrides {
    #[serde(default)]
    autonomy: Option<AutonomyLevel>,
    #[serde(default)]
    workspace_only: Option<bool>,
    #[serde(default)]
    allowed_commands: Option<Vec<String>>,
    #[serde(default)]
    forbidden_paths: Option<Vec<String>>,
    #[serde(default)]
    allowed_roots: Option<Vec<String>>,
    #[serde(default)]
    max_actions_per_hour: Option<u32>,
    #[serde(default)]
    max_cost_per_day_cents: Option<u32>,
    #[serde(default)]
    require_approval_for_medium_risk: Option<bool>,
    #[serde(default)]
    block_high_risk_commands: Option<bool>,
    #[serde(default)]
    shell_env_passthrough: Option<Vec<String>>,
}

pub fn apply_security_plugins(mut policy: SecurityPolicy, workspace_dir: &Path) -> SecurityPolicy {
    let registry = match PluginRegistry::from_runtime(workspace_dir) {
        Ok(registry) => registry,
        Err(error) => {
            tracing::warn!("Unable to load security plugins: {error}");
            return policy;
        }
    };

    for plugin_id in registry.ids_by_kind(PluginKind::Security) {
        let Some(plugin) = registry.get(&plugin_id) else {
            continue;
        };

        match invoke_security_plugin(plugin, &policy) {
            Ok(overrides) => apply_overrides(&mut policy, overrides, workspace_dir),
            Err(error) => {
                tracing::warn!(
                    plugin_id = %plugin.id,
                    "Security plugin failed; keeping built-in policy defaults: {error}"
                );
            }
        }
    }

    policy
}

fn invoke_security_plugin(
    plugin: &ResolvedPlugin,
    policy: &SecurityPolicy,
) -> Result<SecurityPolicyOverrides> {
    let request = SecurityPluginRequest {
        protocol: "zeroclaw-plugin-v1",
        subsystem: "security",
        plugin: plugin.id.clone(),
        operation: "policy_overrides",
        payload: SecurityPolicySnapshot {
            autonomy: policy.autonomy,
            workspace_only: policy.workspace_only,
            allowed_commands: policy.allowed_commands.clone(),
            forbidden_paths: policy.forbidden_paths.clone(),
            allowed_roots: policy
                .allowed_roots
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
            max_actions_per_hour: policy.max_actions_per_hour,
            max_cost_per_day_cents: policy.max_cost_per_day_cents,
            require_approval_for_medium_risk: policy.require_approval_for_medium_risk,
            block_high_risk_commands: policy.block_high_risk_commands,
            shell_env_passthrough: policy.shell_env_passthrough.clone(),
        },
    };
    let request_json = serde_json::to_vec(&request).context("serialize security plugin request")?;

    let mut command = Command::new(&plugin.command);
    command
        .args(&plugin.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in &plugin.env {
        command.env(key, value);
    }

    let mut child = command.spawn().with_context(|| {
        format!(
            "failed to spawn security plugin '{}' with command '{}'",
            plugin.id, plugin.command
        )
    })?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(&request_json)
            .context("write security plugin request")?;
        stdin.write_all(b"\n").context("write newline")?;
        stdin.flush().context("flush security plugin request")?;
    }

    let output = wait_with_timeout(child, Duration::from_secs(plugin.timeout_secs.max(1)))
        .with_context(|| {
            format!(
                "security plugin '{}' timed out after {}s",
                plugin.id, plugin.timeout_secs
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "security plugin '{}' exited with status {}: {}",
            plugin.id,
            output
                .status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_string()),
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        anyhow::bail!("security plugin '{}' returned an empty response", plugin.id);
    }

    let parsed: SecurityPluginResponse<SecurityPolicyOverrides> =
        serde_json::from_str(&stdout).context("decode security plugin response JSON")?;
    if !parsed.ok {
        let detail = parsed
            .error
            .filter(|msg| !msg.trim().is_empty())
            .unwrap_or_else(|| "unknown plugin error".to_string());
        anyhow::bail!(
            "security plugin '{}' failed '{}': {detail}",
            plugin.id,
            request.operation
        );
    }

    Ok(parsed.data.unwrap_or_default())
}

fn wait_with_timeout(mut child: Child, timeout: Duration) -> Result<Output> {
    let started = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output().context("wait for security plugin");
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("security plugin process timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn apply_overrides(
    policy: &mut SecurityPolicy,
    overrides: SecurityPolicyOverrides,
    workspace_dir: &Path,
) {
    if let Some(autonomy) = overrides.autonomy {
        policy.autonomy = autonomy;
    }
    if let Some(workspace_only) = overrides.workspace_only {
        policy.workspace_only = workspace_only;
    }
    if let Some(commands) = overrides.allowed_commands {
        policy.allowed_commands = sanitize_string_list(commands);
    }
    if let Some(paths) = overrides.forbidden_paths {
        policy.forbidden_paths = sanitize_string_list(paths);
    }
    if let Some(roots) = overrides.allowed_roots {
        policy.allowed_roots = roots
            .into_iter()
            .filter_map(|root| normalize_allowed_root(workspace_dir, &root))
            .collect();
    }
    if let Some(limit) = overrides.max_actions_per_hour {
        if limit == 0 {
            tracing::warn!("Security plugin returned max_actions_per_hour=0; ignoring override");
        } else {
            policy.max_actions_per_hour = limit;
        }
    }
    if let Some(limit) = overrides.max_cost_per_day_cents {
        policy.max_cost_per_day_cents = limit;
    }
    if let Some(value) = overrides.require_approval_for_medium_risk {
        policy.require_approval_for_medium_risk = value;
    }
    if let Some(value) = overrides.block_high_risk_commands {
        policy.block_high_risk_commands = value;
    }
    if let Some(keys) = overrides.shell_env_passthrough {
        policy.shell_env_passthrough = sanitize_string_list(keys);
    }
}

fn sanitize_string_list(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

fn normalize_allowed_root(workspace_dir: &Path, value: &str) -> Option<PathBuf> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    let path = expand_user_path(trimmed);
    if path.is_absolute() {
        Some(path)
    } else {
        Some(workspace_dir.join(path))
    }
}

fn expand_user_path(path: &str) -> PathBuf {
    if path == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    }

    if let Some(stripped) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(stripped);
        }
    }

    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PluginKind;
    use crate::plugins::PluginSource;
    use tempfile::tempdir;

    #[test]
    fn apply_overrides_updates_policy_fields() {
        let workspace = tempdir().unwrap();
        let mut policy = SecurityPolicy::default();
        let overrides = SecurityPolicyOverrides {
            autonomy: Some(AutonomyLevel::Full),
            workspace_only: Some(false),
            allowed_commands: Some(vec!["echo".into(), "  ls  ".into()]),
            forbidden_paths: Some(vec!["/etc".into()]),
            allowed_roots: Some(vec!["data".into()]),
            max_actions_per_hour: Some(100),
            max_cost_per_day_cents: Some(42),
            require_approval_for_medium_risk: Some(false),
            block_high_risk_commands: Some(true),
            shell_env_passthrough: Some(vec!["TOKEN".into()]),
        };

        apply_overrides(&mut policy, overrides, workspace.path());

        assert_eq!(policy.autonomy, AutonomyLevel::Full);
        assert!(!policy.workspace_only);
        assert_eq!(policy.allowed_commands, vec!["echo", "ls"]);
        assert_eq!(policy.forbidden_paths, vec!["/etc"]);
        assert_eq!(policy.max_actions_per_hour, 100);
        assert_eq!(policy.max_cost_per_day_cents, 42);
        assert!(!policy.require_approval_for_medium_risk);
        assert!(policy.block_high_risk_commands);
        assert_eq!(policy.shell_env_passthrough, vec!["TOKEN"]);
        assert_eq!(policy.allowed_roots.len(), 1);
        assert!(policy.allowed_roots[0].ends_with("data"));
    }

    #[cfg(unix)]
    #[test]
    fn invoke_security_plugin_reads_json_response() {
        let plugin = ResolvedPlugin {
            id: "sec_guard".to_string(),
            kind: PluginKind::Security,
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "cat >/dev/null; echo '{\"ok\":true,\"data\":{\"max_actions_per_hour\":77}}'"
                    .to_string(),
            ],
            env: std::collections::HashMap::new(),
            timeout_secs: 5,
            source: PluginSource::Config,
        };
        let policy = SecurityPolicy::default();

        let overrides = invoke_security_plugin(&plugin, &policy).unwrap();
        assert_eq!(overrides.max_actions_per_hour, Some(77));
    }
}
