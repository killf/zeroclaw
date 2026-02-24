use super::traits::{Channel, ChannelMessage, SendMessage};
use crate::plugins::ResolvedPlugin;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;

const DEFAULT_LISTEN_POLL_INTERVAL_MS: u64 = 750;

#[derive(Clone)]
pub struct CommandPluginChannel {
    plugin: ResolvedPlugin,
    display_name: String,
    poll_interval: Duration,
}

#[derive(Debug, Serialize)]
struct PluginChannelRequest<T> {
    protocol: &'static str,
    subsystem: &'static str,
    plugin: String,
    operation: &'static str,
    payload: T,
}

#[derive(Debug, Deserialize)]
struct PluginChannelResponse<T> {
    #[serde(default)]
    ok: bool,
    data: Option<T>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PluginInboundMessage {
    #[serde(default)]
    id: Option<String>,
    sender: String,
    #[serde(default)]
    reply_target: Option<String>,
    content: String,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    timestamp: Option<u64>,
    #[serde(default)]
    thread_ts: Option<String>,
}

#[derive(Debug, Serialize)]
struct SendPayload<'a> {
    content: &'a str,
    recipient: &'a str,
    subject: Option<&'a str>,
    thread_ts: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct RecipientPayload<'a> {
    recipient: &'a str,
}

#[derive(Debug, Serialize)]
struct DraftPayload<'a> {
    recipient: &'a str,
    message_id: &'a str,
    text: &'a str,
}

#[derive(Debug, Serialize)]
struct ReactionPayload<'a> {
    channel_id: &'a str,
    message_id: &'a str,
    emoji: &'a str,
}

#[derive(Debug, Serialize)]
struct EmptyPayload {}

impl PluginInboundMessage {
    fn into_channel_message(self, fallback_channel: &str) -> ChannelMessage {
        let timestamp = self.timestamp.unwrap_or_else(unix_timestamp_now);
        let channel = self.channel.unwrap_or_else(|| fallback_channel.to_string());
        let reply_target = self.reply_target.unwrap_or_else(|| self.sender.clone());
        let id = self
            .id
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| format!("{channel}:{timestamp}"));

        ChannelMessage {
            id,
            sender: self.sender,
            reply_target,
            content: self.content,
            channel,
            timestamp,
            thread_ts: self.thread_ts,
        }
    }
}

fn unix_timestamp_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

impl CommandPluginChannel {
    pub fn new(plugin: ResolvedPlugin) -> Self {
        let display_name = format!("plugin:{}", plugin.id);
        Self {
            plugin,
            display_name,
            poll_interval: Duration::from_millis(DEFAULT_LISTEN_POLL_INTERVAL_MS),
        }
    }

    async fn invoke<TReq, TResp>(
        &self,
        operation: &'static str,
        payload: TReq,
    ) -> Result<Option<TResp>>
    where
        TReq: Serialize,
        TResp: DeserializeOwned,
    {
        let request = PluginChannelRequest {
            protocol: "zeroclaw-plugin-v1",
            subsystem: "channel",
            plugin: self.plugin.id.clone(),
            operation,
            payload,
        };
        let request_json =
            serde_json::to_vec(&request).context("serialize channel plugin request")?;

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
                "failed to spawn channel plugin '{}' with command '{}'",
                self.plugin.id, self.plugin.command
            )
        })?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(&request_json)
                .await
                .context("write channel plugin request")?;
            stdin.write_all(b"\n").await.context("write newline")?;
            stdin
                .flush()
                .await
                .context("flush channel plugin request")?;
        }

        let output = tokio::time::timeout(
            Duration::from_secs(self.plugin.timeout_secs.max(1)),
            child.wait_with_output(),
        )
        .await
        .with_context(|| {
            format!(
                "channel plugin '{}' timed out after {}s",
                self.plugin.id, self.plugin.timeout_secs
            )
        })?
        .context("wait for channel plugin process")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "channel plugin '{}' exited with status {}: {}",
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
                "channel plugin '{}' returned an empty response",
                self.plugin.id
            );
        }

        let parsed: PluginChannelResponse<TResp> =
            serde_json::from_str(&stdout).context("decode channel plugin response JSON")?;
        if !parsed.ok {
            let detail = parsed
                .error
                .filter(|msg| !msg.trim().is_empty())
                .unwrap_or_else(|| "unknown plugin error".to_string());
            anyhow::bail!(
                "channel plugin '{}' failed '{}': {detail}",
                self.plugin.id,
                operation
            );
        }

        Ok(parsed.data)
    }
}

#[async_trait]
impl Channel for CommandPluginChannel {
    fn name(&self) -> &str {
        &self.display_name
    }

    async fn send(&self, message: &SendMessage) -> Result<()> {
        let payload = SendPayload {
            content: &message.content,
            recipient: &message.recipient,
            subject: message.subject.as_deref(),
            thread_ts: message.thread_ts.as_deref(),
        };
        let _ = self.invoke::<_, serde_json::Value>("send", payload).await?;
        Ok(())
    }

    async fn listen(&self, tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> Result<()> {
        loop {
            if tx.is_closed() {
                return Ok(());
            }

            let items: Vec<PluginInboundMessage> = self
                .invoke("listen", EmptyPayload {})
                .await?
                .unwrap_or_default();

            for item in items {
                tx.send(item.into_channel_message(&self.display_name))
                    .await
                    .map_err(|error| anyhow::anyhow!("dispatch plugin channel message: {error}"))?;
            }

            tokio::time::sleep(self.poll_interval).await;
        }
    }

    async fn health_check(&self) -> bool {
        self.invoke::<_, bool>("health_check", EmptyPayload {})
            .await
            .ok()
            .flatten()
            .unwrap_or(false)
    }

    async fn start_typing(&self, recipient: &str) -> Result<()> {
        let _ = self
            .invoke::<_, serde_json::Value>("start_typing", RecipientPayload { recipient })
            .await?;
        Ok(())
    }

    async fn stop_typing(&self, recipient: &str) -> Result<()> {
        let _ = self
            .invoke::<_, serde_json::Value>("stop_typing", RecipientPayload { recipient })
            .await?;
        Ok(())
    }

    async fn send_draft(&self, message: &SendMessage) -> Result<Option<String>> {
        let payload = SendPayload {
            content: &message.content,
            recipient: &message.recipient,
            subject: message.subject.as_deref(),
            thread_ts: message.thread_ts.as_deref(),
        };
        self.invoke("send_draft", payload).await
    }

    async fn update_draft(&self, recipient: &str, message_id: &str, text: &str) -> Result<()> {
        let _ = self
            .invoke::<_, serde_json::Value>(
                "update_draft",
                DraftPayload {
                    recipient,
                    message_id,
                    text,
                },
            )
            .await?;
        Ok(())
    }

    async fn finalize_draft(&self, recipient: &str, message_id: &str, text: &str) -> Result<()> {
        let _ = self
            .invoke::<_, serde_json::Value>(
                "finalize_draft",
                DraftPayload {
                    recipient,
                    message_id,
                    text,
                },
            )
            .await?;
        Ok(())
    }

    async fn cancel_draft(&self, recipient: &str, message_id: &str) -> Result<()> {
        let _ = self
            .invoke::<_, serde_json::Value>(
                "cancel_draft",
                DraftPayload {
                    recipient,
                    message_id,
                    text: "",
                },
            )
            .await?;
        Ok(())
    }

    async fn add_reaction(&self, channel_id: &str, message_id: &str, emoji: &str) -> Result<()> {
        let _ = self
            .invoke::<_, serde_json::Value>(
                "add_reaction",
                ReactionPayload {
                    channel_id,
                    message_id,
                    emoji,
                },
            )
            .await?;
        Ok(())
    }

    async fn remove_reaction(&self, channel_id: &str, message_id: &str, emoji: &str) -> Result<()> {
        let _ = self
            .invoke::<_, serde_json::Value>(
                "remove_reaction",
                ReactionPayload {
                    channel_id,
                    message_id,
                    emoji,
                },
            )
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbound_message_defaults_are_filled() {
        let msg = PluginInboundMessage {
            id: None,
            sender: "alice".to_string(),
            reply_target: None,
            content: "hello".to_string(),
            channel: None,
            timestamp: None,
            thread_ts: None,
        };

        let mapped = msg.into_channel_message("plugin:test");
        assert_eq!(mapped.sender, "alice");
        assert_eq!(mapped.reply_target, "alice");
        assert_eq!(mapped.channel, "plugin:test");
        assert!(!mapped.id.is_empty());
        assert!(mapped.timestamp > 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn health_check_invokes_plugin_command() {
        let plugin = ResolvedPlugin {
            id: "health_probe".to_string(),
            kind: crate::config::PluginKind::Channel,
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "cat >/dev/null; echo '{\"ok\":true,\"data\":true}'".to_string(),
            ],
            env: std::collections::HashMap::new(),
            timeout_secs: 5,
            source: crate::plugins::PluginSource::Config,
        };

        let channel = CommandPluginChannel::new(plugin);
        assert!(channel.health_check().await);
    }
}
