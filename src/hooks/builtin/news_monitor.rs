use std::sync::Arc;
use async_trait::async_trait;
use parking_lot::Mutex;
use rusqlite::Connection;
use crate::hooks::traits::HookHandler;
use crate::providers::Provider;

#[derive(Debug, Clone)]
pub struct NewsMonitorConfig {
    pub enabled: bool,
    pub chat_id: String,
    pub check_interval_minutes: u32,
    pub db_path: String,
}

pub struct NewsMonitorHook {
    config: NewsMonitorConfig,
    db: Arc<Mutex<Connection>>,
    http_client: reqwest::Client,
    provider: Arc<dyn Provider>,
    model: String,
}

impl NewsMonitorHook {
    pub fn new(config: NewsMonitorConfig, provider: Arc<dyn Provider>, model: String) -> Result<Self, anyhow::Error> {
        if config.db_path != ":memory:" {
            if let Some(parent) = std::path::Path::new(&config.db_path).parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
        }
        let conn = Connection::open(&config.db_path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS seen_articles (
                url TEXT PRIMARY KEY,
                title TEXT,
                seen_at TEXT
            );
            CREATE TABLE IF NOT EXISTS monitor_state (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );"
        )?;
        Ok(Self {
            config,
            db: Arc::new(Mutex::new(conn)),
            http_client: reqwest::Client::builder()
                .user_agent("Mozilla/5.0 (compatible; zeroclaw-news-monitor/1.0)")
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
            provider,
            model,
        })
    }

    fn is_seen(&self, url: &str) -> bool {
        let db = self.db.lock();
        db.query_row(
            "SELECT 1 FROM seen_articles WHERE url = ?1",
            rusqlite::params![url],
            |_| Ok(1i32),
        ).is_ok()
    }

    fn mark_seen(&self, url: &str, title: &str) -> Result<(), anyhow::Error> {
        let db = self.db.lock();
        let now = chrono::Utc::now().to_rfc3339();
        db.execute(
            "INSERT OR IGNORE INTO seen_articles (url, title, seen_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![url, title, now],
        )?;
        Ok(())
    }

    fn count_seen(&self) -> i64 {
        let db = self.db.lock();
        db.query_row("SELECT COUNT(*) FROM seen_articles", [], |row| row.get(0))
            .unwrap_or(0)
    }

    fn get_state(&self, key: &str) -> Option<String> {
        let db = self.db.lock();
        db.query_row(
            "SELECT value FROM monitor_state WHERE key = ?1",
            rusqlite::params![key],
            |row| row.get(0),
        ).ok()
    }

    fn set_state(&self, key: &str, value: &str) -> Result<(), anyhow::Error> {
        let db = self.db.lock();
        db.execute(
            "INSERT OR REPLACE INTO monitor_state (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    fn tg_token(&self) -> String {
        std::env::var("TG_BOT_TOKEN")
            .unwrap_or_else(|_| "<REDACTED>".to_string())
    }

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
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
        });
        let resp = self.http_client
            .post(format!("https://api.telegram.org/bot{}/sendMessage", self.tg_token()))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let plain_body = serde_json::json!({
                "chat_id": self.config.chat_id,
                "text": text,
                "disable_web_page_preview": true,
            });
            let _ = self.http_client
                .post(format!("https://api.telegram.org/bot{}/sendMessage", self.tg_token()))
                .json(&plain_body)
                .send()
                .await;
        }
        Ok(())
    }

    async fn fetch_article_text(&self, url: &str) -> String {
        let resp = match self.http_client.get(url).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("news_monitor: fetch_article_text failed for {url}: {e}");
                return String::new();
            }
        };
        let html = match resp.text().await {
            Ok(t) => t,
            Err(_) => return String::new(),
        };
        // Strip HTML tags with regex
        let tag_re = regex::Regex::new(r"<[^>]+>").unwrap();
        let text = tag_re.replace_all(&html, " ");
        // Collapse whitespace
        let ws_re = regex::Regex::new(r"\s+").unwrap();
        let text = ws_re.replace_all(&text, " ");
        let text = text.trim().to_string();
        // Truncate to 4000 chars
        if text.len() > 4000 {
            text[..4000].to_string()
        } else {
            text
        }
    }

    async fn fetch_openai_articles(&self) -> Vec<(String, String)> {
        let rss_url = "https://openai.com/news/rss.xml";
        let resp = match self.http_client.get(rss_url).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("news_monitor: OpenAI RSS fetch failed: {e}");
                return vec![];
            }
        };
        let xml = match resp.text().await {
            Ok(t) => t,
            Err(_) => return vec![],
        };

        let item_re = regex::Regex::new(r"(?s)<item>(.*?)</item>").unwrap();
        let title_re = regex::Regex::new(r"<title>(?:<!\[CDATA\[)?(.*?)(?:\]\]>)?</title>").unwrap();
        let link_re = regex::Regex::new(r"<link>\s*(https?://[^<]+?)\s*</link>").unwrap();

        let mut results = vec![];
        for cap in item_re.captures_iter(&xml) {
            let item = &cap[1];
            let title = title_re.captures(item)
                .map(|c| c[1].trim().to_string())
                .unwrap_or_default();
            let link = link_re.captures(item)
                .map(|c| c[1].trim().to_string())
                .unwrap_or_default();
            if !link.is_empty() && !title.is_empty() {
                results.push((link, title));
            }
        }
        results
    }

    async fn fetch_anthropic_articles(&self) -> Vec<(String, String)> {
        let news_url = "https://www.anthropic.com/news";
        let resp = match self.http_client.get(news_url).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("news_monitor: Anthropic news fetch failed: {e}");
                return vec![];
            }
        };
        let html = match resp.text().await {
            Ok(t) => t,
            Err(_) => return vec![],
        };

        let slug_re = regex::Regex::new(r#"href="(/news/[a-z0-9-]+)""#).unwrap();
        let mut seen_slugs = std::collections::HashSet::new();
        let mut results = vec![];

        for cap in slug_re.captures_iter(&html) {
            let slug = cap[1].to_string();
            if !seen_slugs.insert(slug.clone()) {
                continue;
            }
            let url = format!("https://www.anthropic.com{slug}");
            let title = self.fetch_page_title(&url).await;
            results.push((url, title));
        }
        results
    }

    async fn fetch_page_title(&self, url: &str) -> String {
        let resp = match self.http_client.get(url).send().await {
            Ok(r) => r,
            Err(_) => return url.to_string(),
        };
        let html = match resp.text().await {
            Ok(t) => t,
            Err(_) => return url.to_string(),
        };
        let title_re = regex::Regex::new(r"(?s)<title[^>]*>(.*?)</title>").unwrap();
        if let Some(cap) = title_re.captures(&html) {
            let raw = cap[1].trim().to_string();
            let clean = raw.split(" | ").next().unwrap_or(&raw).trim().to_string();
            if !clean.is_empty() {
                return clean;
            }
        }
        url.to_string()
    }

    async fn summarize_article(&self, source: &str, title: &str, url: &str) -> String {
        let article_text = self.fetch_article_text(url).await;
        if article_text.is_empty() {
            return String::new();
        }
        let prompt = format!(
            "以下是 {source} 發布的文章「{title}」的內容。用繁體中文做重點整理，輸出 Telegram HTML 格式。\n\
            要求：1) 一句話摘要 2) 3-5 個重點用 • 列點 3) 技術名詞用 <code> 標籤 4) 不超過 300 字\n\
            只用 <b> 和 <code> 標籤。\n\n\
            {article_text}"
        );
        match self.provider.simple_chat(&prompt, &self.model, 0.7).await {
            Ok(text) => text.trim().to_string(),
            Err(e) => {
                tracing::warn!("news_monitor: AI summary failed for {url}: {e}");
                String::new()
            }
        }
    }

    async fn process_articles(&self, source: &str, articles: Vec<(String, String)>) -> Result<(), anyhow::Error> {
        for (url, title) in articles {
            if self.is_seen(&url) {
                continue;
            }
            self.mark_seen(&url, &title)?;
            tracing::info!("news_monitor: new article from {source}: {title}");
            let summary = self.summarize_article(source, &title, &url).await;
            let message = if summary.is_empty() {
                format!("📰 <b>{source}</b>\n<b>{title}</b>\n\n🔗 {url}")
            } else {
                format!("📰 <b>{source}</b>\n<b>{title}</b>\n\n{summary}\n\n🔗 {url}")
            };
            if let Err(e) = self.send_tg_html(&message).await {
                tracing::warn!("news_monitor: failed to send TG message: {e}");
            }
            // Small delay between notifications to avoid TG rate limits
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        }
        Ok(())
    }

    async fn check_news(&self) -> Result<(), anyhow::Error> {
        let is_first_run = self.count_seen() == 0;
        if is_first_run {
            tracing::info!("news_monitor: first run detected, caching existing articles without notifying");
        }

        let openai_articles = self.fetch_openai_articles().await;
        let anthropic_articles = self.fetch_anthropic_articles().await;

        if is_first_run {
            let total = openai_articles.len() + anthropic_articles.len();
            for (url, title) in &openai_articles {
                let _ = self.mark_seen(url, title);
            }
            for (url, title) in &anthropic_articles {
                let _ = self.mark_seen(url, title);
            }
            tracing::info!("news_monitor: first run, caching {total} articles without notifying");
            return Ok(());
        }

        if let Err(e) = self.process_articles("OpenAI", openai_articles).await {
            tracing::warn!("news_monitor: error processing OpenAI articles: {e}");
        }
        if let Err(e) = self.process_articles("Anthropic", anthropic_articles).await {
            tracing::warn!("news_monitor: error processing Anthropic articles: {e}");
        }
        Ok(())
    }

    pub async fn background_tick(&self) -> Result<(), anyhow::Error> {
        let now_epoch = chrono::Utc::now().timestamp() as u64;
        let next_check = self.get_state("next_news_check")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        if now_epoch >= next_check {
            self.check_news().await?;
            let interval = self.config.check_interval_minutes as u64 * 60;
            self.set_state("next_news_check", &(now_epoch + interval).to_string())?;
        }
        Ok(())
    }
}

#[async_trait]
impl HookHandler for NewsMonitorHook {
    fn name(&self) -> &str { "news_monitor" }

    async fn on_gateway_start(&self, _host: &str, _port: u16) {
        if !self.config.enabled {
            return;
        }
        tracing::info!("📰 NewsMonitorHook registered (interval: {}min)",
            self.config.check_interval_minutes);

        let config = self.config.clone();
        let db = Arc::clone(&self.db);
        let http_client = self.http_client.clone();
        let provider = Arc::clone(&self.provider);
        let model = self.model.clone();

        tokio::spawn(async move {
            let hook = NewsMonitorHook { config, db, http_client, provider, model };
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                if let Err(e) = hook.background_tick().await {
                    tracing::warn!("news_monitor tick error: {e}");
                }
            }
        });
    }
}
