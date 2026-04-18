use crate::hooks::traits::HookHandler;
use crate::providers::Provider;
use async_trait::async_trait;
use parking_lot::Mutex;
use rusqlite::Connection;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub(super) const SECTION_NAME: &str = "email_monitor";

fn default_email_check_interval() -> u32 {
    3
}
fn default_email_db_path() -> String {
    "/home/azureuser/.email-monitor/emails.db".to_string()
}
fn default_email_blocked_senders() -> Vec<String> {
    vec![
        "noreply-apps-scripts-notifications@google.com".to_string(),
        "messages-noreply@linkedin.com".to_string(),
        "no-reply@linebizsolutions.com".to_string(),
    ]
}

/// Configuration for the email-monitor builtin hook.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EmailMonitorConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub chat_id: String,
    #[serde(default = "default_email_check_interval")]
    pub check_interval_minutes: u32,
    #[serde(default = "default_email_db_path")]
    pub db_path: String,
    #[serde(default = "default_email_blocked_senders")]
    pub blocked_senders: Vec<String>,
}

impl Default for EmailMonitorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            chat_id: String::new(),
            check_interval_minutes: default_email_check_interval(),
            db_path: default_email_db_path(),
            blocked_senders: default_email_blocked_senders(),
        }
    }
}

pub struct EmailMonitorHook {
    config: EmailMonitorConfig,
    db: Arc<Mutex<Connection>>,
    http_client: reqwest::Client,
    provider: Arc<dyn Provider>,
    model: String,
    identity: String,
}

impl EmailMonitorHook {
    pub fn new(
        config: EmailMonitorConfig,
        identity: String,
        provider: Arc<dyn Provider>,
        model: String,
    ) -> Result<Self, anyhow::Error> {
        if config.db_path != ":memory:" {
            if let Some(parent) = std::path::Path::new(&config.db_path).parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
        }
        let conn = Connection::open(&config.db_path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS notified_emails (
                message_id TEXT PRIMARY KEY,
                notified_at TEXT
            );
            CREATE TABLE IF NOT EXISTS monitor_state (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )?;
        Ok(Self {
            config,
            db: Arc::new(Mutex::new(conn)),
            http_client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
            provider,
            model,
            identity,
        })
    }

    fn is_notified(&self, message_id: &str) -> bool {
        let db = self.db.lock();
        db.query_row(
            "SELECT 1 FROM notified_emails WHERE message_id = ?1",
            rusqlite::params![message_id],
            |_| Ok(1i32),
        )
        .is_ok()
    }

    fn mark_notified(&self, message_id: &str) -> Result<(), anyhow::Error> {
        let db = self.db.lock();
        let now = chrono::Utc::now().to_rfc3339();
        db.execute(
            "INSERT OR IGNORE INTO notified_emails (message_id, notified_at) VALUES (?1, ?2)",
            rusqlite::params![message_id, now],
        )?;
        Ok(())
    }

    fn count_notified(&self) -> i64 {
        let db = self.db.lock();
        db.query_row("SELECT COUNT(*) FROM notified_emails", [], |row| row.get(0))
            .unwrap_or(0)
    }

    fn get_state(&self, key: &str) -> Option<String> {
        let db = self.db.lock();
        db.query_row(
            "SELECT value FROM monitor_state WHERE key = ?1",
            rusqlite::params![key],
            |row| row.get(0),
        )
        .ok()
    }

    fn set_state(&self, key: &str, value: &str) -> Result<(), anyhow::Error> {
        let db = self.db.lock();
        db.execute(
            "INSERT OR REPLACE INTO monitor_state (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    fn cleanup_old_entries(&self) {
        let db = self.db.lock();
        if let Err(e) = db.execute(
            "DELETE FROM notified_emails WHERE notified_at < datetime('now', '-30 days')",
            [],
        ) {
            tracing::warn!("email_monitor: cleanup failed: {e}");
        }
    }

    fn line_token(&self) -> String {
        if let Ok(token) = std::env::var("LINE_CHANNEL_ACCESS_TOKEN") {
            if !token.is_empty() {
                return token;
            }
        }
        // Fallback: parse channel_access_token from ~/.zeroclaw/config.toml
        let home = std::env::var("HOME").unwrap_or_default();
        let config_path = format!("{home}/.zeroclaw/config.toml");
        if let Ok(content) = std::fs::read_to_string(&config_path) {
            for line in content.lines() {
                let line = line.trim();
                if line.starts_with("channel_access_token") {
                    if let Some(val) = line.splitn(2, '=').nth(1) {
                        return val.trim().trim_matches('"').to_string();
                    }
                }
            }
        }
        String::new()
    }

    async fn send_line_push(&self, text: &str) -> Result<(), anyhow::Error> {
        let token = self.line_token();
        if token.is_empty() {
            anyhow::bail!("email_monitor: LINE token not configured");
        }
        let group_id = "Cb7deac0f0caf563051dde48fb710cc33";
        let body = serde_json::json!({
            "to": group_id,
            "messages": [{"type": "text", "text": text}]
        });
        let resp = self
            .http_client
            .post("https://api.line.me/v2/bot/message/push")
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            tracing::warn!("email_monitor: LINE push failed: {status} {body_text}");
        }
        Ok(())
    }

    fn is_blocked_sender(&self, from: &str) -> bool {
        let from_lower = from.to_lowercase();
        self.config
            .blocked_senders
            .iter()
            .any(|blocked| from_lower.contains(&blocked.to_lowercase()))
    }

    /// Fetch full email content via gws CLI, return plain text body (up to 3000 chars).
    async fn fetch_email_content(&self, message_id: &str) -> String {
        let params = serde_json::json!({
            "id": message_id,
            "userId": "me",
            "format": "full"
        });
        let output = tokio::process::Command::new("gws")
            .args([
                "gmail",
                "users",
                "messages",
                "get",
                "--params",
                &params.to_string(),
                "--format",
                "json",
            ])
            .env("GOOGLE_WORKSPACE_CLI_KEYRING_BACKEND", "file")
            .output()
            .await;

        let output = match output {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!("email_monitor: fetch_email_content gws failed: {e}");
                return String::new();
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let json_start = stdout.find('{').unwrap_or(0);
        let json_str = &stdout[json_start..];

        let data: serde_json::Value = match serde_json::from_str(json_str) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("email_monitor: failed to parse email JSON: {e}");
                return String::new();
            }
        };

        let body_text = Self::extract_body_text(&data);
        if body_text.len() > 3000 {
            body_text[..3000].to_string()
        } else {
            body_text
        }
    }

    fn extract_body_text(data: &serde_json::Value) -> String {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;

        fn walk(
            part: &serde_json::Value,
            plain: &mut Option<String>,
            html: &mut Option<String>,
            b64: &base64::engine::general_purpose::GeneralPurpose,
        ) {
            let mime = part.get("mimeType").and_then(|v| v.as_str()).unwrap_or("");
            if mime == "text/plain" || mime == "text/html" {
                if let Some(data_b64) = part
                    .get("body")
                    .and_then(|b| b.get("data"))
                    .and_then(|d| d.as_str())
                {
                    if let Ok(bytes) = b64.decode(data_b64) {
                        if let Ok(text) = String::from_utf8(bytes) {
                            if mime == "text/plain" && plain.is_none() {
                                *plain = Some(text);
                            } else if mime == "text/html" && html.is_none() {
                                *html = Some(text);
                            }
                        }
                    }
                }
            }
            if let Some(parts) = part.get("parts").and_then(|p| p.as_array()) {
                for sub in parts {
                    walk(sub, plain, html, b64);
                }
            }
        }

        let mut plain: Option<String> = None;
        let mut html: Option<String> = None;
        let payload = &data["payload"];
        walk(payload, &mut plain, &mut html, &b64);

        if let Some(text) = plain {
            return text.trim().to_string();
        }
        if let Some(html_text) = html {
            return Self::strip_html(&html_text);
        }
        String::new()
    }

    fn strip_html(html: &str) -> String {
        let re_script = regex::Regex::new(r"(?is)<(script|style)[^>]*>.*?</\1>").unwrap();
        let text = re_script.replace_all(html, " ");
        let re_tags = regex::Regex::new(r"<[^>]+>").unwrap();
        let text = re_tags.replace_all(&text, " ");
        let text = text
            .replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&nbsp;", " ")
            .replace("&quot;", "\"")
            .replace("&#39;", "'");
        let re_ws = regex::Regex::new(r"\s{2,}").unwrap();
        re_ws.replace_all(text.trim(), " ").to_string()
    }

    async fn check_emails(&self) -> Result<(), anyhow::Error> {
        let output = tokio::process::Command::new("gws")
            .args(["gmail", "+triage", "--format", "json"])
            .env("GOOGLE_WORKSPACE_CLI_KEYRING_BACKEND", "file")
            .output()
            .await;

        let output = match output {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!("email_monitor: failed to run gws: {e}");
                return Ok(());
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim().is_empty() {
            tracing::debug!("email_monitor: gws returned empty output");
            return Ok(());
        }

        let json_start = stdout.find('{').unwrap_or(0);
        let json_str = &stdout[json_start..];

        let parsed: serde_json::Value = match serde_json::from_str(json_str) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("email_monitor: failed to parse gws JSON: {e}\nOutput: {stdout}");
                return Ok(());
            }
        };

        let messages = match parsed.get("messages").and_then(|m| m.as_array()) {
            Some(arr) => arr.clone(),
            None => {
                tracing::debug!("email_monitor: no messages key in gws output");
                return Ok(());
            }
        };

        let is_first_run = self.count_notified() == 0;
        if is_first_run {
            tracing::info!(
                "email_monitor: first run, caching {} messages without notifying",
                messages.len()
            );
            for msg in &messages {
                if let Some(id) = msg.get("id").and_then(|v| v.as_str()) {
                    let _ = self.mark_notified(id);
                }
            }
            return Ok(());
        }

        for msg in &messages {
            let id = match msg.get("id").and_then(|v| v.as_str()) {
                Some(v) => v,
                None => continue,
            };
            let from = msg
                .get("from")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            let subject = msg
                .get("subject")
                .and_then(|v| v.as_str())
                .unwrap_or("(no subject)");

            if self.is_notified(id) {
                continue;
            }

            if self.is_blocked_sender(from) {
                tracing::debug!("email_monitor: skipping blocked sender: {from}");
                self.mark_notified(id)?;
                continue;
            }

            tracing::info!("email_monitor: new email from {from}: {subject}");

            // Fetch full email body for AI context
            let body_text = self.fetch_email_content(id).await;

            // Generate AI summary in 小允子 style
            let prompt = format!(
                "你是小允子，主子的貼心助理。主子收到一封新郵件，請用你的風格簡短報告重點。\n\n寄件人：{from}\n主旨：{subject}\n內容：\n{body}\n\n用50字以內報告這封信的重點，口語化，帶一個emoji。只輸出報告。",
                from = from,
                subject = subject,
                body = body_text,
            );
            let summary = self
                .provider
                .chat_with_system(Some(&self.identity), &prompt, &self.model, 0.7)
                .await
                .unwrap_or_else(|_| format!("主子，{} 寄了封信來：{}", from, subject));

            let message = format!(
                "\u{1f4e7} {summary}\n\n寄件人：{from}\n主旨：{subject}",
                summary = summary,
                from = from,
                subject = subject,
            );

            if let Err(e) = self.send_line_push(&message).await {
                tracing::warn!("email_monitor: failed to send LINE notification: {e}");
            }

            self.mark_notified(id)?;

            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        }

        Ok(())
    }

    pub async fn background_tick(&self) -> Result<(), anyhow::Error> {
        let now_epoch = chrono::Utc::now().timestamp() as u64;
        let next_check = self
            .get_state("next_email_check")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        if now_epoch >= next_check {
            self.check_emails().await?;
            let interval = self.config.check_interval_minutes as u64 * 60;
            self.set_state("next_email_check", &(now_epoch + interval).to_string())?;

            let last_cleanup = self
                .get_state("last_cleanup")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            if now_epoch >= last_cleanup + 86400 {
                self.cleanup_old_entries();
                self.set_state("last_cleanup", &now_epoch.to_string())?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl HookHandler for EmailMonitorHook {
    fn name(&self) -> &str {
        "email_monitor"
    }

    async fn on_gateway_start(&self, _host: &str, _port: u16) {
        if !self.config.enabled {
            return;
        }
        tracing::info!(
            "\u{1f4e7} EmailMonitorHook registered (interval: {}min)",
            self.config.check_interval_minutes
        );

        let config = self.config.clone();
        let db = Arc::clone(&self.db);
        let http_client = self.http_client.clone();
        let provider = Arc::clone(&self.provider);
        let model = self.model.clone();
        let identity = self.identity.clone();

        tokio::spawn(async move {
            let hook = EmailMonitorHook {
                config,
                db,
                http_client,
                provider,
                model,
                identity,
            };
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                if let Err(e) = hook.background_tick().await {
                    tracing::warn!("email_monitor tick error: {e}");
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
    crate::extra_hook_lookup!(
        config.hooks.builtin.extra,
        SECTION_NAME,
        EmailMonitorConfig,
        em_config
    );
    let identity =
        std::fs::read_to_string(config.workspace_dir.join("IDENTITY.md")).unwrap_or_default();
    match EmailMonitorHook::new(em_config, identity, Arc::clone(provider), model.to_string()) {
        Ok(hook) => {
            runner.register(Box::new(hook));
            tracing::info!("Email monitor hook registered");
        }
        Err(e) => tracing::warn!("Failed to initialize email monitor: {e}"),
    }
}
