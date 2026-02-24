pub mod registry;

use crate::config::Config;
use anyhow::Result;

/// Integration status
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum IntegrationStatus {
    /// Fully implemented and ready to use
    Available,
    /// Configured and active
    Active,
    /// Planned but not yet implemented
    ComingSoon,
}

/// Integration category
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum IntegrationCategory {
    Chat,
    AiModel,
    Productivity,
    MusicAudio,
    SmartHome,
    ToolsAutomation,
    MediaCreative,
    Social,
    Platform,
}

impl IntegrationCategory {
    pub fn label(self) -> &'static str {
        match self {
            Self::Chat => "Chat Providers",
            Self::AiModel => "AI Models",
            Self::Productivity => "Productivity",
            Self::MusicAudio => "Music & Audio",
            Self::SmartHome => "Smart Home",
            Self::ToolsAutomation => "Tools & Automation",
            Self::MediaCreative => "Media & Creative",
            Self::Social => "Social",
            Self::Platform => "Platforms",
        }
    }

    pub fn all() -> &'static [Self] {
        &[
            Self::Chat,
            Self::AiModel,
            Self::Productivity,
            Self::MusicAudio,
            Self::SmartHome,
            Self::ToolsAutomation,
            Self::MediaCreative,
            Self::Social,
            Self::Platform,
        ]
    }
}

/// A registered integration
pub struct IntegrationEntry {
    pub name: &'static str,
    pub description: &'static str,
    pub category: IntegrationCategory,
    pub status_fn: fn(&Config) -> IntegrationStatus,
}

/// Handle the `integrations` CLI command
pub fn handle_command(command: crate::IntegrationCommands, config: &Config) -> Result<()> {
    match command {
        crate::IntegrationCommands::List { category, status } => {
            list_integrations(config, category.as_deref(), status.as_deref())
        }
        crate::IntegrationCommands::Search { query } => search_integrations(config, &query),
        crate::IntegrationCommands::Info { name } => show_integration_info(config, &name),
    }
}

fn status_icon(status: IntegrationStatus) -> (&'static str, &'static str) {
    match status {
        IntegrationStatus::Active => ("✅", "Active"),
        IntegrationStatus::Available => ("⚪", "Available"),
        IntegrationStatus::ComingSoon => ("🔜", "Coming Soon"),
    }
}

fn normalize_filter_value(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

fn parse_category_filter(value: &str) -> Result<IntegrationCategory> {
    let normalized = normalize_filter_value(value);
    match normalized.as_str() {
        "chat" | "chats" | "chatprovider" | "chatproviders" => Ok(IntegrationCategory::Chat),
        "ai" | "model" | "models" | "aimodel" | "aimodels" => Ok(IntegrationCategory::AiModel),
        "productivity" => Ok(IntegrationCategory::Productivity),
        "music" | "audio" | "musicaudio" => Ok(IntegrationCategory::MusicAudio),
        "smarthome" | "home" => Ok(IntegrationCategory::SmartHome),
        "tools" | "automation" | "toolsautomation" => Ok(IntegrationCategory::ToolsAutomation),
        "media" | "creative" | "mediacreative" => Ok(IntegrationCategory::MediaCreative),
        "social" => Ok(IntegrationCategory::Social),
        "platform" | "platforms" => Ok(IntegrationCategory::Platform),
        _ => anyhow::bail!(
            "Unknown category filter: {value}. Valid options: chat, ai-models, productivity, music-audio, smart-home, tools-automation, media-creative, social, platforms."
        ),
    }
}

fn parse_status_filter(value: &str) -> Result<IntegrationStatus> {
    let normalized = normalize_filter_value(value);
    match normalized.as_str() {
        "active" => Ok(IntegrationStatus::Active),
        "available" => Ok(IntegrationStatus::Available),
        "comingsoon" | "planned" => Ok(IntegrationStatus::ComingSoon),
        _ => anyhow::bail!(
            "Unknown status filter: {value}. Valid options: active, available, coming-soon."
        ),
    }
}

fn filtered_integrations<'a>(
    entries: &'a [IntegrationEntry],
    config: &Config,
    category_filter: Option<IntegrationCategory>,
    status_filter: Option<IntegrationStatus>,
    search_query: Option<&str>,
) -> Vec<(&'a IntegrationEntry, IntegrationStatus)> {
    let search = search_query
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .map(str::to_ascii_lowercase);

    entries
        .iter()
        .filter_map(|entry| {
            let status = (entry.status_fn)(config);
            if category_filter.is_some_and(|category| entry.category != category) {
                return None;
            }
            if status_filter.is_some_and(|wanted| status != wanted) {
                return None;
            }

            if let Some(query) = search.as_deref() {
                let name = entry.name.to_ascii_lowercase();
                let description = entry.description.to_ascii_lowercase();
                if !name.contains(query) && !description.contains(query) {
                    return None;
                }
            }

            Some((entry, status))
        })
        .collect()
}

fn list_integrations(config: &Config, category: Option<&str>, status: Option<&str>) -> Result<()> {
    let category_filter = category.map(parse_category_filter).transpose()?;
    let status_filter = status.map(parse_status_filter).transpose()?;

    let entries = registry::all_integrations();
    let filtered = filtered_integrations(&entries, config, category_filter, status_filter, None);

    println!();
    if filtered.is_empty() {
        println!("  No integrations matched the selected filters.");
        println!();
        return Ok(());
    }

    for category in IntegrationCategory::all() {
        let in_category: Vec<_> = filtered
            .iter()
            .filter(|(entry, _)| entry.category == *category)
            .collect();
        if in_category.is_empty() {
            continue;
        }

        println!("  {}", console::style(category.label()).white().bold());
        for (entry, status) in in_category {
            let (icon, label) = status_icon(*status);
            println!(
                "    {icon} {} ({label}) — {}",
                console::style(entry.name).white().bold(),
                entry.description
            );
        }
        println!();
    }

    Ok(())
}

fn search_integrations(config: &Config, query: &str) -> Result<()> {
    if query.trim().is_empty() {
        anyhow::bail!("Search query cannot be empty.");
    }

    let entries = registry::all_integrations();
    let matches = filtered_integrations(&entries, config, None, None, Some(query));

    println!();
    if matches.is_empty() {
        println!("  No integrations matched \"{query}\".");
        println!("  Try a broader keyword or run `zeroclaw integrations list`.");
        println!();
        return Ok(());
    }

    println!("  Search results for \"{query}\":");
    println!();
    for (entry, status) in matches {
        let (icon, status_label) = status_icon(status);
        println!(
            "  {icon} {} [{} • {}] — {}",
            console::style(entry.name).white().bold(),
            entry.category.label(),
            status_label,
            entry.description
        );
    }
    println!();

    Ok(())
}

fn show_integration_info(config: &Config, name: &str) -> Result<()> {
    let entries = registry::all_integrations();
    let name_lower = name.to_lowercase();

    let Some(entry) = entries.iter().find(|e| e.name.to_lowercase() == name_lower) else {
        anyhow::bail!(
            "Unknown integration: {name}. Check README for supported integrations or run `zeroclaw onboard --interactive` to configure channels/providers."
        );
    };

    let status = (entry.status_fn)(config);
    let (icon, label) = status_icon(status);

    println!();
    println!(
        "  {} {} — {}",
        icon,
        console::style(entry.name).white().bold(),
        entry.description
    );
    println!("  Category: {}", entry.category.label());
    println!("  Status:   {label}");
    println!();

    // Show setup hints based on integration
    match entry.name {
        "Telegram" => {
            println!("  Setup:");
            println!("    1. Message @BotFather on Telegram");
            println!("    2. Create a bot and copy the token");
            println!("    3. Run: zeroclaw onboard --channels-only");
            println!("    4. Start: zeroclaw channel start");
        }
        "Discord" => {
            println!("  Setup:");
            println!("    1. Go to https://discord.com/developers/applications");
            println!("    2. Create app → Bot → Copy token");
            println!("    3. Enable MESSAGE CONTENT intent");
            println!("    4. Run: zeroclaw onboard --channels-only");
        }
        "Slack" => {
            println!("  Setup:");
            println!("    1. Go to https://api.slack.com/apps");
            println!("    2. Create app → Bot Token Scopes → Install");
            println!("    3. Run: zeroclaw onboard --channels-only");
        }
        "OpenRouter" => {
            println!("  Setup:");
            println!("    1. Get API key at https://openrouter.ai/keys");
            println!("    2. Run: zeroclaw onboard");
            println!("    Access 200+ models with one key.");
        }
        "Ollama" => {
            println!("  Setup:");
            println!("    1. Install: brew install ollama");
            println!("    2. Pull a model: ollama pull llama3");
            println!("    3. Set provider to 'ollama' in config.toml");
        }
        "iMessage" => {
            println!("  Setup (macOS only):");
            println!("    Uses AppleScript bridge to send/receive iMessages.");
            println!("    Requires Full Disk Access in System Settings → Privacy.");
        }
        "GitHub" => {
            println!("  Setup:");
            println!("    1. Create a personal access token at https://github.com/settings/tokens");
            println!("    2. Add to config: [integrations.github] token = \"ghp_...\"");
        }
        "Browser" => {
            println!("  Built-in:");
            println!("    ZeroClaw can control Chrome/Chromium for web tasks.");
            println!("    Uses headless browser automation.");
        }
        "Cron" => {
            println!("  Built-in:");
            println!("    Schedule tasks in ~/.zeroclaw/workspace/cron/");
            println!("    Run: zeroclaw cron list");
        }
        "Webhooks" => {
            println!("  Built-in:");
            println!("    HTTP endpoint for external triggers.");
            println!("    Run: zeroclaw gateway");
        }
        _ => {
            if status == IntegrationStatus::ComingSoon {
                println!("  This integration is planned. Stay tuned!");
                println!("  Track progress: https://github.com/theonlyhennygod/zeroclaw");
            }
        }
    }

    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn always_available(_: &Config) -> IntegrationStatus {
        IntegrationStatus::Available
    }

    fn always_active(_: &Config) -> IntegrationStatus {
        IntegrationStatus::Active
    }

    fn always_coming_soon(_: &Config) -> IntegrationStatus {
        IntegrationStatus::ComingSoon
    }

    #[test]
    fn integration_category_all_includes_every_variant_once() {
        let all = IntegrationCategory::all();
        assert_eq!(all.len(), 9);

        let labels: Vec<&str> = all.iter().map(|cat| cat.label()).collect();
        assert!(labels.contains(&"Chat Providers"));
        assert!(labels.contains(&"AI Models"));
        assert!(labels.contains(&"Productivity"));
        assert!(labels.contains(&"Music & Audio"));
        assert!(labels.contains(&"Smart Home"));
        assert!(labels.contains(&"Tools & Automation"));
        assert!(labels.contains(&"Media & Creative"));
        assert!(labels.contains(&"Social"));
        assert!(labels.contains(&"Platforms"));
    }

    #[test]
    fn handle_command_info_is_case_insensitive_for_known_integrations() {
        let config = Config::default();
        let first_name = registry::all_integrations()
            .first()
            .expect("registry should define at least one integration")
            .name
            .to_lowercase();

        let result = handle_command(
            crate::IntegrationCommands::Info { name: first_name },
            &config,
        );

        assert!(result.is_ok());
    }

    #[test]
    fn handle_command_info_returns_error_for_unknown_integration() {
        let config = Config::default();
        let result = handle_command(
            crate::IntegrationCommands::Info {
                name: "definitely-not-a-real-integration".into(),
            },
            &config,
        );

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Unknown integration"));
    }

    #[test]
    fn parse_category_filter_accepts_common_aliases() {
        assert_eq!(
            parse_category_filter("ai").expect("ai should parse"),
            IntegrationCategory::AiModel
        );
        assert_eq!(
            parse_category_filter("models").expect("models should parse"),
            IntegrationCategory::AiModel
        );
        assert_eq!(
            parse_category_filter("ai-models").expect("ai-models should parse"),
            IntegrationCategory::AiModel
        );
        assert_eq!(
            parse_category_filter("tools").expect("tools should parse"),
            IntegrationCategory::ToolsAutomation
        );
        assert_eq!(
            parse_category_filter("chat").expect("chat should parse"),
            IntegrationCategory::Chat
        );
    }

    #[test]
    fn parse_category_filter_rejects_unknown_value_with_options() {
        let err = parse_category_filter("unknown-category")
            .expect_err("unknown category should return an error")
            .to_string();
        assert!(err.contains("Unknown category filter"));
        assert!(err.contains("Valid options"));
    }

    #[test]
    fn parse_status_filter_accepts_supported_values() {
        assert_eq!(
            parse_status_filter("active").expect("active should parse"),
            IntegrationStatus::Active
        );
        assert_eq!(
            parse_status_filter("available").expect("available should parse"),
            IntegrationStatus::Available
        );
        assert_eq!(
            parse_status_filter("coming-soon").expect("coming-soon should parse"),
            IntegrationStatus::ComingSoon
        );
    }

    #[test]
    fn parse_status_filter_rejects_unknown_value_with_options() {
        let err = parse_status_filter("beta")
            .expect_err("unknown status should return an error")
            .to_string();
        assert!(err.contains("Unknown status filter"));
        assert!(err.contains("Valid options"));
    }

    #[test]
    fn filtered_integrations_apply_category_status_and_keyword_filters() {
        let entries = vec![
            IntegrationEntry {
                name: "Alpha Chat",
                description: "chat integration",
                category: IntegrationCategory::Chat,
                status_fn: always_available,
            },
            IntegrationEntry {
                name: "Beta Model",
                description: "model tooling",
                category: IntegrationCategory::AiModel,
                status_fn: always_active,
            },
            IntegrationEntry {
                name: "Gamma Tools",
                description: "automation workbench",
                category: IntegrationCategory::ToolsAutomation,
                status_fn: always_coming_soon,
            },
        ];
        let config = Config::default();

        let active = filtered_integrations(
            &entries,
            &config,
            None,
            Some(IntegrationStatus::Active),
            None,
        );
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].0.name, "Beta Model");

        let ai_only = filtered_integrations(
            &entries,
            &config,
            Some(IntegrationCategory::AiModel),
            None,
            None,
        );
        assert_eq!(ai_only.len(), 1);
        assert_eq!(ai_only[0].0.name, "Beta Model");

        let searched = filtered_integrations(&entries, &config, None, None, Some("AUTOMATION"));
        assert_eq!(searched.len(), 1);
        assert_eq!(searched[0].0.name, "Gamma Tools");
    }

    #[test]
    fn search_command_handles_no_results_gracefully() {
        let config = Config::default();
        let result = handle_command(
            crate::IntegrationCommands::Search {
                query: "definitely-no-match".into(),
            },
            &config,
        );

        assert!(result.is_ok());
    }

    #[test]
    fn list_command_rejects_invalid_filters() {
        let config = Config::default();
        let bad_category = handle_command(
            crate::IntegrationCommands::List {
                category: Some("not-real".into()),
                status: None,
            },
            &config,
        )
        .expect_err("invalid category should error")
        .to_string();
        assert!(bad_category.contains("Unknown category filter"));

        let bad_status = handle_command(
            crate::IntegrationCommands::List {
                category: None,
                status: Some("not-real".into()),
            },
            &config,
        )
        .expect_err("invalid status should error")
        .to_string();
        assert!(bad_status.contains("Unknown status filter"));
    }
}
