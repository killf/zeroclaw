use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChannelHealthState {
    Healthy,
    Unhealthy,
    Timeout,
}

pub(super) fn classify_health_result(
    result: &std::result::Result<bool, tokio::time::error::Elapsed>,
) -> ChannelHealthState {
    match result {
        Ok(true) => ChannelHealthState::Healthy,
        Ok(false) => ChannelHealthState::Unhealthy,
        Err(_) => ChannelHealthState::Timeout,
    }
}

pub(super) struct ConfiguredChannel {
    pub(super) display_name: &'static str,
    pub(super) channel: Arc<dyn Channel>,
}

pub(super) fn collect_configured_channels(
    config: &Config,
    _matrix_skip_context: &str,
) -> Vec<ConfiguredChannel> {
    let mut channels = Vec::new();

    if let Some(ref tg) = config.channels_config.telegram {
        let mut telegram_channel = TelegramChannel::new(
            tg.bot_token.clone(),
            tg.allowed_users.clone(),
            tg.mention_only,
        )
        .with_streaming(tg.stream_mode, tg.draft_update_interval_ms)
        .with_transcription(config.transcription.clone())
        .with_workspace_dir(config.workspace_dir.clone());
        if let Some(base_url) = tg.base_url.as_ref() {
            telegram_channel = telegram_channel.with_api_base(base_url.clone());
        }

        channels.push(ConfiguredChannel {
            display_name: "Telegram",
            channel: Arc::new(telegram_channel),
        });
    }

    if let Some(ref dc) = config.channels_config.discord {
        channels.push(ConfiguredChannel {
            display_name: "Discord",
            channel: Arc::new(DiscordChannel::new(
                dc.bot_token.clone(),
                dc.guild_id.clone(),
                dc.allowed_users.clone(),
                dc.listen_to_bots,
                dc.mention_only,
            )),
        });
    }

    if let Some(ref sl) = config.channels_config.slack {
        channels.push(ConfiguredChannel {
            display_name: "Slack",
            channel: Arc::new(SlackChannel::new(
                sl.bot_token.clone(),
                sl.channel_id.clone(),
                sl.allowed_users.clone(),
            )),
        });
    }

    if let Some(ref mm) = config.channels_config.mattermost {
        channels.push(ConfiguredChannel {
            display_name: "Mattermost",
            channel: Arc::new(MattermostChannel::new(
                mm.url.clone(),
                mm.bot_token.clone(),
                mm.channel_id.clone(),
                mm.allowed_users.clone(),
                mm.thread_replies.unwrap_or(true),
                mm.mention_only.unwrap_or(false),
            )),
        });
    }

    if let Some(ref im) = config.channels_config.imessage {
        channels.push(ConfiguredChannel {
            display_name: "iMessage",
            channel: Arc::new(IMessageChannel::new(im.allowed_contacts.clone())),
        });
    }

    #[cfg(feature = "channel-matrix")]
    if let Some(ref mx) = config.channels_config.matrix {
        channels.push(ConfiguredChannel {
            display_name: "Matrix",
            channel: Arc::new(MatrixChannel::new_with_session_hint_and_zeroclaw_dir(
                mx.homeserver.clone(),
                mx.access_token.clone(),
                mx.room_id.clone(),
                mx.allowed_users.clone(),
                mx.user_id.clone(),
                mx.device_id.clone(),
                config.config_path.parent().map(|path| path.to_path_buf()),
            )),
        });
    }

    #[cfg(not(feature = "channel-matrix"))]
    if config.channels_config.matrix.is_some() {
        tracing::warn!(
            "Matrix channel is configured but this build was compiled without `channel-matrix`; skipping Matrix {}.",
            _matrix_skip_context
        );
    }

    if let Some(ref sig) = config.channels_config.signal {
        let mut signal_channel = SignalChannel::new(
            sig.http_url.clone(),
            sig.account.clone(),
            sig.group_id.clone(),
            sig.allowed_from.clone(),
            sig.ignore_attachments,
            sig.ignore_stories,
        );
        if let Some(config_dir) = config.config_path.parent() {
            let pin_manager =
                signal_pin::SignalPinManager::new(sig, config_dir, config.secrets.encrypt);
            signal_channel = signal_channel.with_pin_manager(pin_manager);
        }
        channels.push(ConfiguredChannel {
            display_name: "Signal",
            channel: Arc::new(signal_channel),
        });
    }

    if let Some(ref wa) = config.channels_config.whatsapp {
        if wa.is_ambiguous_config() {
            tracing::warn!(
                "WhatsApp config has both phone_number_id and session_path set; preferring Cloud API mode. Remove one selector to avoid ambiguity."
            );
        }
        // Runtime negotiation: detect backend type from config
        match wa.backend_type() {
            "cloud" => {
                // Cloud API mode: requires phone_number_id, access_token, verify_token
                if wa.is_cloud_config() {
                    channels.push(ConfiguredChannel {
                        display_name: "WhatsApp",
                        channel: Arc::new(WhatsAppChannel::new(
                            wa.access_token.clone().unwrap_or_default(),
                            wa.phone_number_id.clone().unwrap_or_default(),
                            wa.verify_token.clone().unwrap_or_default(),
                            wa.allowed_numbers.clone(),
                        )),
                    });
                } else {
                    tracing::warn!("WhatsApp Cloud API configured but missing required fields (phone_number_id, access_token, verify_token)");
                }
            }
            "web" => {
                // Web mode: requires session_path
                #[cfg(feature = "whatsapp-web")]
                if wa.is_web_config() {
                    channels.push(ConfiguredChannel {
                        display_name: "WhatsApp",
                        channel: Arc::new(WhatsAppWebChannel::new(
                            wa.session_path.clone().unwrap_or_default(),
                            wa.pair_phone.clone(),
                            wa.pair_code.clone(),
                            wa.allowed_numbers.clone(),
                        )),
                    });
                } else {
                    tracing::warn!("WhatsApp Web configured but session_path not set");
                }
                #[cfg(not(feature = "whatsapp-web"))]
                {
                    tracing::warn!("WhatsApp Web backend requires 'whatsapp-web' feature. Enable with: cargo build --features whatsapp-web");
                }
            }
            _ => {
                tracing::warn!("WhatsApp config invalid: neither phone_number_id (Cloud API) nor session_path (Web) is set");
            }
        }
    }

    if let Some(ref lq) = config.channels_config.linq {
        channels.push(ConfiguredChannel {
            display_name: "Linq",
            channel: Arc::new(LinqChannel::new(
                lq.api_token.clone(),
                lq.from_phone.clone(),
                lq.allowed_senders.clone(),
            )),
        });
    }

    if let Some(ref nc) = config.channels_config.nextcloud_talk {
        channels.push(ConfiguredChannel {
            display_name: "Nextcloud Talk",
            channel: Arc::new(NextcloudTalkChannel::new(
                nc.base_url.clone(),
                nc.app_token.clone(),
                nc.allowed_users.clone(),
            )),
        });
    }

    if let Some(ref email_cfg) = config.channels_config.email {
        channels.push(ConfiguredChannel {
            display_name: "Email",
            channel: Arc::new(EmailChannel::new(email_cfg.clone())),
        });
    }

    if let Some(ref irc) = config.channels_config.irc {
        channels.push(ConfiguredChannel {
            display_name: "IRC",
            channel: Arc::new(IrcChannel::new(irc::IrcChannelConfig {
                server: irc.server.clone(),
                port: irc.port,
                nickname: irc.nickname.clone(),
                username: irc.username.clone(),
                channels: irc.channels.clone(),
                allowed_users: irc.allowed_users.clone(),
                server_password: irc.server_password.clone(),
                nickserv_password: irc.nickserv_password.clone(),
                sasl_password: irc.sasl_password.clone(),
                verify_tls: irc.verify_tls.unwrap_or(true),
            })),
        });
    }

    #[cfg(feature = "channel-lark")]
    if let Some(ref lk) = config.channels_config.lark {
        channels.push(ConfiguredChannel {
            display_name: "Lark",
            channel: Arc::new(LarkChannel::from_config(lk)),
        });
    }

    #[cfg(not(feature = "channel-lark"))]
    if config.channels_config.lark.is_some() {
        tracing::warn!(
            "Lark channel is configured but this build was compiled without `channel-lark`; skipping Lark health check."
        );
    }

    if let Some(ref dt) = config.channels_config.dingtalk {
        channels.push(ConfiguredChannel {
            display_name: "DingTalk",
            channel: Arc::new(DingTalkChannel::new(
                dt.client_id.clone(),
                dt.client_secret.clone(),
                dt.allowed_users.clone(),
            )),
        });
    }

    if let Some(ref qq) = config.channels_config.qq {
        channels.push(ConfiguredChannel {
            display_name: "QQ",
            channel: Arc::new(QQChannel::new(
                qq.app_id.clone(),
                qq.app_secret.clone(),
                qq.allowed_users.clone(),
            )),
        });
    }

    if let Some(ref ct) = config.channels_config.clawdtalk {
        channels.push(ConfiguredChannel {
            display_name: "ClawdTalk",
            channel: Arc::new(ClawdTalkChannel::new(ct.clone())),
        });
    }

    match PluginRegistry::from_runtime(&config.workspace_dir) {
        Ok(registry) => {
            for plugin_id in registry.ids_by_kind(crate::config::PluginKind::Channel) {
                if let Some(resolved) = registry.get(&plugin_id) {
                    channels.push(ConfiguredChannel {
                        display_name: "Plugin Channel",
                        channel: Arc::new(plugin::CommandPluginChannel::new(resolved.clone())),
                    });
                }
            }
        }
        Err(error) => {
            tracing::warn!("Failed to load channel plugins: {error}");
        }
    }

    channels
}

/// Run health checks for configured channels.
pub(super) async fn doctor_channels(config: Config) -> Result<()> {
    let mut channels = collect_configured_channels(&config, "health check");

    if let Some(ref ns) = config.channels_config.nostr {
        channels.push(ConfiguredChannel {
            display_name: "Nostr",
            channel: Arc::new(
                NostrChannel::new(&ns.private_key, ns.relays.clone(), &ns.allowed_pubkeys).await?,
            ),
        });
    }

    if channels.is_empty() {
        println!("No real-time channels configured. Run `zeroclaw onboard` first.");
        return Ok(());
    }

    println!("🩺 ZeroClaw Channel Doctor");
    println!();

    let mut healthy = 0_u32;
    let mut unhealthy = 0_u32;
    let mut timeout = 0_u32;

    for configured in channels {
        let result =
            tokio::time::timeout(Duration::from_secs(10), configured.channel.health_check()).await;
        let state = classify_health_result(&result);

        match state {
            ChannelHealthState::Healthy => {
                healthy += 1;
                println!("  ✅ {:<9} healthy", configured.display_name);
            }
            ChannelHealthState::Unhealthy => {
                unhealthy += 1;
                println!(
                    "  ❌ {:<9} unhealthy (auth/config/network)",
                    configured.display_name
                );
            }
            ChannelHealthState::Timeout => {
                timeout += 1;
                println!("  ⏱️  {:<9} timed out (>10s)", configured.display_name);
            }
        }
    }

    if config.channels_config.webhook.is_some() {
        println!("  ℹ️  Webhook   check via `zeroclaw gateway` then GET /health");
    }

    println!();
    println!("Summary: {healthy} healthy, {unhealthy} unhealthy, {timeout} timed out");
    Ok(())
}
