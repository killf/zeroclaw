use super::traits::{Memory, MemoryCategory, MemoryEntry};
use crate::plugins::ResolvedPlugin;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;

#[derive(Clone)]
pub struct CommandPluginMemory {
    plugin: ResolvedPlugin,
    display_name: String,
}

#[derive(Debug, Serialize)]
struct PluginMemoryRequest<'a, T> {
    protocol: &'static str,
    subsystem: &'static str,
    plugin: &'a str,
    operation: &'a str,
    payload: T,
}

#[derive(Debug, Deserialize)]
struct PluginMemoryResponse<T> {
    #[serde(default)]
    ok: bool,
    data: Option<T>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct StorePayload<'a> {
    key: &'a str,
    content: &'a str,
    category: MemoryCategory,
    session_id: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct RecallPayload<'a> {
    query: &'a str,
    limit: usize,
    session_id: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct KeyPayload<'a> {
    key: &'a str,
}

#[derive(Debug, Serialize)]
struct ListPayload<'a> {
    category: Option<&'a MemoryCategory>,
    session_id: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct EmptyPayload {}

impl CommandPluginMemory {
    pub fn new(plugin: ResolvedPlugin) -> Self {
        let display_name = format!("plugin:{}", plugin.id);
        Self {
            plugin,
            display_name,
        }
    }

    async fn invoke<TReq, TResp>(&self, operation: &str, payload: TReq) -> Result<Option<TResp>>
    where
        TReq: Serialize,
        TResp: DeserializeOwned,
    {
        let request = PluginMemoryRequest {
            protocol: "zeroclaw-plugin-v1",
            subsystem: "memory",
            plugin: &self.plugin.id,
            operation,
            payload,
        };
        let request_json = serde_json::to_vec(&request).context("serialize plugin request")?;

        let mut command = tokio::process::Command::new(&self.plugin.command);
        command
            .args(&self.plugin.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in &self.plugin.env {
            command.env(key, value);
        }

        let mut child = command.spawn().with_context(|| {
            format!(
                "failed to spawn memory plugin '{}' with command '{}'",
                self.plugin.id, self.plugin.command
            )
        })?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(&request_json)
                .await
                .context("write plugin request")?;
            stdin.write_all(b"\n").await.context("write newline")?;
            stdin.flush().await.context("flush plugin request")?;
        }

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(self.plugin.timeout_secs.max(1)),
            child.wait_with_output(),
        )
        .await
        .with_context(|| {
            format!(
                "memory plugin '{}' timed out after {}s",
                self.plugin.id, self.plugin.timeout_secs
            )
        })?
        .context("wait for plugin process")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "memory plugin '{}' exited with status {}: {}",
                self.plugin.id,
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
            anyhow::bail!(
                "memory plugin '{}' returned an empty response",
                self.plugin.id
            );
        }

        let parsed: PluginMemoryResponse<TResp> =
            serde_json::from_str(&stdout).context("decode plugin response JSON")?;
        if !parsed.ok {
            let detail = parsed
                .error
                .filter(|msg| !msg.trim().is_empty())
                .unwrap_or_else(|| "unknown plugin error".to_string());
            anyhow::bail!(
                "memory plugin '{}' failed '{}': {detail}",
                self.plugin.id,
                operation
            );
        }

        Ok(parsed.data)
    }
}

#[async_trait]
impl Memory for CommandPluginMemory {
    fn name(&self) -> &str {
        &self.display_name
    }

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> Result<()> {
        let payload = StorePayload {
            key,
            content,
            category,
            session_id,
        };
        let _ = self
            .invoke::<_, serde_json::Value>("store", payload)
            .await?;
        Ok(())
    }

    async fn recall(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
    ) -> Result<Vec<MemoryEntry>> {
        let payload = RecallPayload {
            query,
            limit,
            session_id,
        };
        Ok(self.invoke("recall", payload).await?.unwrap_or_default())
    }

    async fn get(&self, key: &str) -> Result<Option<MemoryEntry>> {
        self.invoke("get", KeyPayload { key }).await
    }

    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> Result<Vec<MemoryEntry>> {
        let payload = ListPayload {
            category,
            session_id,
        };
        Ok(self.invoke("list", payload).await?.unwrap_or_default())
    }

    async fn forget(&self, key: &str) -> Result<bool> {
        Ok(self
            .invoke("forget", KeyPayload { key })
            .await?
            .unwrap_or(false))
    }

    async fn count(&self) -> Result<usize> {
        Ok(self.invoke("count", EmptyPayload {}).await?.unwrap_or(0))
    }

    async fn health_check(&self) -> bool {
        self.invoke::<_, bool>("health_check", EmptyPayload {})
            .await
            .ok()
            .flatten()
            .unwrap_or(false)
    }
}
