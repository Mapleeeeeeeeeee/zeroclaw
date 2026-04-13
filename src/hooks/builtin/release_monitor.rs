use std::sync::Arc;
use async_trait::async_trait;
use parking_lot::Mutex;
use rusqlite::Connection;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use crate::hooks::traits::HookHandler;
use crate::providers::Provider;

pub(super) const SECTION_NAME: &str = "release_monitor";

fn default_release_check_interval() -> u32 { 60 }
fn default_release_db_path() -> String { "/home/azureuser/.release-monitor/releases.db".to_string() }
fn default_release_repos() -> Vec<String> {
    vec![
        "anthropics/claude-code".to_string(),
        "google-gemini/gemini-cli".to_string(),
        "openai/codex".to_string(),
        "github/copilot-cli".to_string(),
    ]
}

/// Configuration for the release-monitor builtin hook.
///
/// When enabled, polls GitHub releases for the configured repos at a regular
/// interval and sends Telegram notifications with AI-generated summaries.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReleaseMonitorConfig {
    /// Enable the release-monitor hook. Default: .
    #[serde(default)]
    pub enabled: bool,
    /// Telegram chat ID to send release notifications to.
    #[serde(default)]
    pub chat_id: String,
    /// How often to check for new releases, in minutes. Default: .
    #[serde(default = "default_release_check_interval")]
    pub check_interval_minutes: u32,
    /// Path to the SQLite database file used for caching seen release tags.
    #[serde(default = "default_release_db_path")]
    pub db_path: String,
    /// GitHub repos to watch for new releases (format: "owner/name").
    #[serde(default = "default_release_repos")]
    pub repos: Vec<String>,
}

impl Default for ReleaseMonitorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            chat_id: String::new(),
            check_interval_minutes: default_release_check_interval(),
            db_path: default_release_db_path(),
            repos: default_release_repos(),
        }
    }
}

pub struct ReleaseMonitorHook {
    config: ReleaseMonitorConfig,
    db: Arc<Mutex<Connection>>,
    http_client: reqwest::Client,
    provider: Arc<dyn Provider>,
    model: String,
    identity: String,
    tg_token: String,
}

impl ReleaseMonitorHook {
    pub fn new(config: ReleaseMonitorConfig, workspace_dir: &std::path::Path, identity: String, provider: Arc<dyn Provider>, model: String) -> Result<Self, anyhow::Error> {
        if config.db_path != ":memory:" {
            if let Some(parent) = std::path::Path::new(&config.db_path).parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
        }
        let conn = Connection::open(&config.db_path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS release_cache (
                repo TEXT PRIMARY KEY,
                tag TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS monitor_state (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );"
        )?;
        let tg_token = super::env_loader::load_env_value(workspace_dir, "TG_BOT_TOKEN")?;
        Ok(Self { config, db: Arc::new(Mutex::new(conn)), http_client: reqwest::Client::new(), provider, model, identity, tg_token })
    }

    fn get_cached_tag(&self, repo: &str) -> Option<String> {
        let db = self.db.lock();
        db.query_row("SELECT tag FROM release_cache WHERE repo = ?1",
            rusqlite::params![repo], |row| row.get(0)).ok()
    }

    fn set_cached_tag(&self, repo: &str, tag: &str) -> Result<(), anyhow::Error> {
        let db = self.db.lock();
        db.execute("INSERT OR REPLACE INTO release_cache (repo, tag) VALUES (?1, ?2)",
            rusqlite::params![repo, tag])?;
        Ok(())
    }

    fn get_state(&self, key: &str) -> Option<String> {
        let db = self.db.lock();
        db.query_row("SELECT value FROM monitor_state WHERE key = ?1",
            rusqlite::params![key], |row| row.get(0)).ok()
    }

    fn set_state(&self, key: &str, value: &str) -> Result<(), anyhow::Error> {
        let db = self.db.lock();
        db.execute("INSERT OR REPLACE INTO monitor_state (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value])?;
        Ok(())
    }

    fn tg_token(&self) -> &str {
        &self.tg_token
    }

    /// Sanitize HTML: escape all & < >, then restore allowed tags
    fn sanitize_html(text: &str) -> String {
        let mut s = text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
        for tag in ["b", "code", "i"] {
            s = s.replace(&format!("&lt;{tag}&gt;"), &format!("<{tag}>"));
            s = s.replace(&format!("&lt;/{tag}&gt;"), &format!("</{tag}>"));
        }
        s
    }

    async fn send_tg_html(&self, text: &str) -> Result<(), anyhow::Error> {
        let sanitized = Self::sanitize_html(text);
        let body = serde_json::json!({
            "chat_id": self.config.chat_id,
            "text": sanitized,
            "parse_mode": "HTML"
        });
        let resp = self.http_client
            .post(format!("https://api.telegram.org/bot{}/sendMessage", self.tg_token()))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            // Fallback: send without HTML
            let plain_body = serde_json::json!({
                "chat_id": self.config.chat_id,
                "text": text,
            });
            let _ = self.http_client
                .post(format!("https://api.telegram.org/bot{}/sendMessage", self.tg_token()))
                .json(&plain_body)
                .send()
                .await;
        }
        Ok(())
    }

    async fn check_releases(&self) -> Result<(), anyhow::Error> {
        let futures: Vec<_> = self.config.repos.iter().map(|repo| self.check_repo(repo)).collect();
        let results = futures_util::future::join_all(futures).await;
        for (repo, result) in self.config.repos.iter().zip(results) {
            if let Err(e) = result {
                tracing::warn!("release_monitor: error checking {repo}: {e}");
            }
        }
        Ok(())
    }

    async fn check_repo(&self, repo: &str) -> Result<(), anyhow::Error> {
        // Fetch latest release from GitHub
        let url = format!("https://api.github.com/repos/{repo}/releases/latest");
        let resp = self.http_client
            .get(&url)
            .header("User-Agent", "zeroclaw-release-monitor/1.0")
            .send()
            .await?;

        if !resp.status().is_success() {
            return Ok(()); // Skip if API fails
        }

        let data: serde_json::Value = resp.json().await?;
        let latest = data["tag_name"].as_str().unwrap_or("");
        if latest.is_empty() {
            return Ok(());
        }

        let cached = self.get_cached_tag(repo);

        // Always update cache
        self.set_cached_tag(repo, latest)?;

        // Only notify if there's a cached version AND it's different
        let Some(old_tag) = cached else {
            tracing::info!("release_monitor: first check for {repo}, cached {latest}");
            return Ok(());
        };

        if old_tag == latest {
            return Ok(());
        }

        tracing::info!("release_monitor: {repo} updated {old_tag} → {latest}");

        // Get changelog
        let changelog = data["body"].as_str().unwrap_or("");
        let changelog_truncated = &changelog[..changelog.len().min(3000)];

        // Generate AI summary using internal Provider
        let prompt = format!(
            "以下是 {repo} 從 {old_tag} 更新到 {latest} 的 changelog。用繁體中文分三個區塊摘要，輸出必須是 Telegram HTML 格式。\n\n\
            格式範例（嚴格遵守）：\n\n\
            🔒 <b>安全/Breaking Changes</b>\n\
            • 項目描述\n\
            （沒有就寫「無」）\n\n\
            ✨ <b>新功能</b>\n\
            • 項目描述，技術名詞用 <code>xxx</code> 包起來\n\n\
            🔧 <b>重要修復</b>\n\
            • 項目描述，每項精簡一行\n\
            • 及其他 N 項修復（如果還有的話）\n\n\
            規則：\n\
            - 只用 <b> 和 <code> 標籤，不要用其他 HTML 標籤\n\
            - 不要用 Markdown（不要用 ** 或 `）\n\
            - 每項用 • 開頭\n\
            - 每項一行，精簡扼要（10-15 字以內）\n\
            - 不要編造不存在的項目\n\
            - 修復最多列 8 項，其餘用「及其他 N 項修復」帶過\n\n\
            {changelog_truncated}"
        );

        let summary = match self.provider.chat_with_system(Some(&self.identity), &prompt, &self.model, 0.7).await {
            Ok(text) => text.trim().to_string(),
            Err(e) => {
                tracing::warn!("release_monitor: AI summary failed for {repo}: {e}");
                String::new()
            }
        };

        let header = format!("🆕 <b>{repo}</b> 更新了！<code>{old_tag}</code> → <code>{latest}</code>");
        let message = if summary.is_empty() {
            header
        } else {
            format!("{header}\n\n{summary}")
        };

        self.send_tg_html(&message).await?;
        Ok(())
    }

    pub async fn background_tick(&self) -> Result<(), anyhow::Error> {
        let now_epoch = chrono::Utc::now().timestamp() as u64;
        let next_check = self.get_state("next_check")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        if now_epoch >= next_check {
            self.check_releases().await?;
            let interval = self.config.check_interval_minutes as u64 * 60;
            self.set_state("next_check", &(now_epoch + interval).to_string())?;
        }

        Ok(())
    }
}

#[async_trait]
impl HookHandler for ReleaseMonitorHook {
    fn name(&self) -> &str { "release_monitor" }

    async fn on_gateway_start(&self, _host: &str, _port: u16) {
        if !self.config.enabled {
            return;
        }
        tracing::info!("📦 ReleaseMonitorHook registered (interval: {}min, repos: {})",
            self.config.check_interval_minutes, self.config.repos.len());

        let config = self.config.clone();
        let db = Arc::clone(&self.db);
        let http_client = self.http_client.clone();
        let provider = Arc::clone(&self.provider);
        let model = self.model.clone();
        let identity = self.identity.clone();
        let tg_token = self.tg_token.clone();

        tokio::spawn(async move {
            let hook = ReleaseMonitorHook { config, db, http_client, provider, model, identity, tg_token };
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                if let Err(e) = hook.background_tick().await {
                    tracing::warn!("release_monitor tick error: {e}");
                }
            }
        });
    }
}

pub fn register(
    runner: &mut crate::hooks::HookRunner,
    config: &crate::config::schema::Config,
    provider: &std::sync::Arc<dyn crate::providers::Provider>,
    model: &str,
) {
    crate::extra_hook_lookup!(config.hooks.builtin.extra, SECTION_NAME, ReleaseMonitorConfig, rm_config);
    let identity = std::fs::read_to_string(config.workspace_dir.join("IDENTITY.md"))
        .unwrap_or_default();
    match ReleaseMonitorHook::new(rm_config, &config.workspace_dir, identity, Arc::clone(provider), model.to_string()) {
        Ok(hook) => {
            runner.register(Box::new(hook));
            tracing::info!("📦 Release monitor hook registered");
        }
        Err(e) => tracing::warn!("Failed to initialize release monitor: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_monitor_default_repos_includes_all_four_cli_tools() {
        let config = ReleaseMonitorConfig::default();
        assert_eq!(config.repos.len(), 4, "expected 4 default repos, got {}: {:?}", config.repos.len(), config.repos);
        assert!(config.repos.contains(&"anthropics/claude-code".to_string()));
        assert!(config.repos.contains(&"google-gemini/gemini-cli".to_string()));
        assert!(config.repos.contains(&"openai/codex".to_string()));
        assert!(config.repos.contains(&"github/copilot-cli".to_string()),
            "github/copilot-cli must be in defaults");
    }

    #[test]
    fn release_monitor_config_accepts_custom_repo_list() {
        let toml = r#"
            enabled = true
            chat_id = "-1234567890"
            check_interval_minutes = 30
            db_path = "/tmp/test.db"
            repos = ["foo/bar", "baz/qux"]
        "#;
        let config: ReleaseMonitorConfig = toml::from_str(toml).expect("parse");
        assert_eq!(config.repos, vec!["foo/bar".to_string(), "baz/qux".to_string()]);
        assert_eq!(config.check_interval_minutes, 30);
    }

    #[test]
    fn release_monitor_config_omitting_repos_falls_back_to_default() {
        let toml = r#"
            enabled = true
            chat_id = "-1"
        "#;
        let config: ReleaseMonitorConfig = toml::from_str(toml).expect("parse");
        // When `repos` is omitted, serde should use default_release_repos().
        assert_eq!(config.repos.len(), 4);
        assert!(config.repos.contains(&"github/copilot-cli".to_string()));
    }

    #[test]
    fn release_monitor_parses_from_builtin_hooks_extra() {
        let toml = r#"
[hooks]
enabled = true

[hooks.builtin]
command_logger = true

[hooks.builtin.release_monitor]
enabled = true
chat_id = "-1"
"#;
        let parsed: crate::config::schema::Config = toml::from_str(toml).expect("parse full config");
        let rm_value = parsed.hooks.builtin.extra.get("release_monitor")
            .expect("release_monitor section must land in extra");
        let rm: ReleaseMonitorConfig = rm_value.clone().try_into().expect("decode config");
        assert!(rm.enabled);
        assert_eq!(rm.chat_id, "-1");
    }
}
