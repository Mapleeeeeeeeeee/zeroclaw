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
    identity: String,
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
        let identity = std::fs::read_to_string("/home/azureuser/.zeroclaw/workspace/IDENTITY.md")
            .unwrap_or_default();
        Ok(Self {
            config,
            db: Arc::new(Mutex::new(conn)),
            http_client: reqwest::Client::builder()
                .user_agent("Mozilla/5.0 (compatible; zeroclaw-news-monitor/1.0)")
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
            provider,
            model,
            identity,
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
        Self::parse_openai_rss(&xml)
    }

    /// Parses an OpenAI RSS feed body and returns `(link, title)` pairs,
    /// filtering out OpenAI Academy help pages. Pure — no I/O.
    fn parse_openai_rss(xml: &str) -> Vec<(String, String)> {
        let item_re = regex::Regex::new(r"(?s)<item>(.*?)</item>").unwrap();
        let title_re = regex::Regex::new(r"<title>(?:<!\[CDATA\[)?(.*?)(?:\]\]>)?</title>").unwrap();
        let link_re = regex::Regex::new(r"<link>\s*(https?://[^<]+?)\s*</link>").unwrap();

        let mut results = vec![];
        for cap in item_re.captures_iter(xml) {
            let item = &cap[1];
            let title = title_re.captures(item)
                .map(|c| c[1].trim().to_string())
                .unwrap_or_default();
            let link = link_re.captures(item)
                .map(|c| c[1].trim().to_string())
                .unwrap_or_default();
            if link.is_empty() || title.is_empty() {
                continue;
            }
            // Skip OpenAI Academy help pages — they pollute the feed but aren't news.
            if link.contains("/academy/") {
                continue;
            }
            results.push((link, title));
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

        let urls = Self::parse_anthropic_slugs(&html);
        let mut results = vec![];
        for url in urls {
            let title = self.fetch_page_title(&url).await;
            results.push((url, title));
        }
        results
    }

    /// Parses Anthropic /news HTML and returns deduplicated absolute article URLs
    /// in their order of first appearance. Pure — no I/O.
    ///
    /// Accepts both relative (`href="/news/foo"`) and absolute
    /// (`href="https://www.anthropic.com/news/foo"`) hrefs, and allows
    /// uppercase/underscore in slugs (e.g. `australia-MOU`).
    fn parse_anthropic_slugs(html: &str) -> Vec<String> {
        let slug_re = regex::Regex::new(
            r#"href="(?:https?://[^"]*?)?/news/([a-zA-Z0-9_-]+)""#
        ).unwrap();
        let mut seen = std::collections::HashSet::new();
        let mut urls = vec![];
        for cap in slug_re.captures_iter(html) {
            let slug = &cap[1];
            let url = format!("https://www.anthropic.com/news/{slug}");
            if seen.insert(url.clone()) {
                urls.push(url);
            }
        }
        urls
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
        match self.provider.chat_with_system(Some(&self.identity), &prompt, &self.model, 0.7).await {
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
        if let Err(e) = self.check_dia_changelog().await {
            tracing::warn!("news_monitor: error checking Dia changelog: {e}");
        }
        Ok(())
    }


    /// Extracts a release identifier from Jina-rendered Dia release-notes markdown.
    /// Skips the page title (H1 containing "Release Notes" or "|") and returns
    /// the next H1. Returns an empty string if no release heading is found.
    fn parse_dia_release_id(markdown: &str) -> String {
        markdown.lines()
            .filter(|l| l.starts_with("# "))
            .find(|l| !l.contains("Release Notes") && !l.contains('|'))
            .map(|l| l.trim().to_string())
            .unwrap_or_default()
    }

    async fn check_dia_changelog(&self) -> Result<(), anyhow::Error> {
        // Use Jina Reader to bypass Cloudflare
        let jina_url = "https://r.jina.ai/https://www.diabrowser.com/release-notes/latest";
        let resp = match self.http_client
            .get(jina_url)
            .header("Accept", "text/plain")
            .send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("dia_monitor: fetch error: {e}");
                return Ok(());
            }
        };
        let text = resp.text().await.unwrap_or_default();
        if text.is_empty() || text.len() < 50 {
            tracing::warn!("dia_monitor: empty or too short response");
            return Ok(());
        }

        let release_id = Self::parse_dia_release_id(&text);
        if release_id.is_empty() {
            tracing::warn!("dia_monitor: could not find release heading");
            return Ok(());
        }

        // Compare with DB
        let last_id = self.get_state("dia_last_release_id").unwrap_or_default();
        if last_id == release_id {
            return Ok(()); // No new release
        }

        // First run: silently cache
        if last_id.is_empty() {
            tracing::info!("dia_monitor: first run, caching release: {release_id}");
            self.set_state("dia_last_release_id", &release_id)?;
            return Ok(());
        }

        tracing::info!("dia_monitor: new release detected: {release_id} (was: {last_id})");

        // Truncate content for LLM
        let content_for_llm: String = text.chars().take(2000).collect();

        // LLM judges if target features exist
        let prompt = format!(
            "以下是 Dia 瀏覽器的最新更新日誌：\n\n{}\n\n使用者想知道有沒有以下功能：\n1. 左右滑動切換 Space（Spaces 功能，類似 Arc 的 Space，可以用手勢左右滑動切換不同工作空間）\n2. 底部快速換列（底部有列表或 dock 可以快速切換 Space/Tab Group）\n\n如果有任何一個相關功能，用繁體中文回覆具體更新了什麼（50字以內）。如果完全沒有相關功能，只回覆 NONE 這個字。",
            content_for_llm
        );

        let reply = match self.provider.chat_with_system(
            Some(&self.identity), &prompt, &self.model, 0.3
        ).await {
            Ok(r) => r.trim().to_string(),
            Err(e) => {{
                tracing::warn!("dia_monitor: LLM error: {{e}}");
                self.set_state("dia_last_release_id", &release_id)?;
                return Ok(());
            }}
        };

        // Update DB
        self.set_state("dia_last_release_id", &release_id)?;

        // Check if should notify
        let trimmed = reply.trim();
        if trimmed == "NONE" || trimmed.is_empty() || trimmed.starts_with("NONE") {
            tracing::info!("dia_monitor: no target features in this release, skipping");
            return Ok(());
        }

        // Send notification
        let message = format!(
            "🌐 <b>Dia Browser 更新：你要的功能來了！</b>\n\n{}\n\n🔗 https://www.diabrowser.com/release-notes/latest",
            trimmed
        );
        self.send_tg_html(&message).await;
        tracing::info!("dia_monitor: sent notification for release: {release_id}");

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
        let identity = self.identity.clone();

        tokio::spawn(async move {
            let hook = NewsMonitorHook { config, db, http_client, provider, model, identity };
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                if let Err(e) = hook.background_tick().await {
                    tracing::warn!("news_monitor tick error: {e}");
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANTHROPIC_FIXTURE: &str = include_str!("../../../tests/fixtures/anthropic_news.html");
    const OPENAI_RSS_FIXTURE: &str = include_str!("../../../tests/fixtures/openai_news_rss.xml");
    const DIA_NOTES_FIXTURE: &str = include_str!("../../../tests/fixtures/dia_release_notes.md");

    #[test]
    fn filters_out_openai_academy_help_pages() {
        let results = NewsMonitorHook::parse_openai_rss(OPENAI_RSS_FIXTURE);
        // Fixture has 20 items: 19 /academy/ pages + 1 /index/ real news.
        assert_eq!(
            results.len(), 1,
            "expected 1 non-academy item from fixture, got {}: {results:#?}",
            results.len()
        );
        let (link, title) = &results[0];
        assert!(
            link.contains("/index/"),
            "surviving item should be an /index/ news page, got link={link}"
        );
        assert!(
            !link.contains("/academy/"),
            "academy pages must be filtered out; got {link}"
        );
        assert!(
            !title.is_empty(),
            "surviving item must have a non-empty title"
        );
    }

    #[test]
    fn openai_rss_parser_skips_items_with_empty_title_or_link() {
        let xml = r#"
            <rss><channel>
                <item><title>Only title</title></item>
                <item><link>https://openai.com/index/only-link</link></item>
                <item>
                    <title><![CDATA[Real Item]]></title>
                    <link>https://openai.com/index/real</link>
                </item>
                <item>
                    <title><![CDATA[Academy Noise]]></title>
                    <link>https://openai.com/academy/noise</link>
                </item>
            </channel></rss>
        "#;
        let results = NewsMonitorHook::parse_openai_rss(xml);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "https://openai.com/index/real");
        assert_eq!(results[0].1, "Real Item");
    }

    #[test]
    fn dia_release_parser_skips_page_title_and_returns_release_heading() {
        let release_id = NewsMonitorHook::parse_dia_release_id(DIA_NOTES_FIXTURE);
        // Real fixture's release title is "# Pick up where you left off" (Sync feature).
        // Must NOT be the page title "# Dia Browser | Latest Release Notes".
        assert_eq!(release_id, "# Pick up where you left off");
        assert!(!release_id.contains("Release Notes"));
        assert!(!release_id.contains('|'));
    }

    #[test]
    fn dia_release_parser_handles_missing_heading() {
        let empty_md = "just some text without any headings\nmore text\n";
        let release_id = NewsMonitorHook::parse_dia_release_id(empty_md);
        assert_eq!(release_id, "");
    }

    #[test]
    fn dia_release_parser_uses_h1_not_h2() {
        // Regression guard: old code used `## ` and would have returned "## Past Issues".
        let md = "\
# Some Release Title\n\
some content\n\
## Past Issues\n\
- old release 1\n\
- old release 2\n\
";
        let release_id = NewsMonitorHook::parse_dia_release_id(md);
        assert_eq!(release_id, "# Some Release Title");
        assert_ne!(release_id, "## Past Issues");
    }

    #[test]
    fn parses_anthropic_slugs_from_real_fixture() {
        let urls = NewsMonitorHook::parse_anthropic_slugs(ANTHROPIC_FIXTURE);

        // Baseline: should find exactly the 14 unique articles currently on the news index.
        assert_eq!(urls.len(), 14, "expected 14 unique articles, got {}: {urls:#?}", urls.len());

        // Regression guard #1: slug with uppercase letters must be captured in full.
        // Previously [a-z0-9-]+ truncated "australia-MOU" to "australia-" → 404.
        assert!(
            urls.contains(&"https://www.anthropic.com/news/australia-MOU".to_string()),
            "missing australia-MOU (uppercase slug); urls = {urls:#?}"
        );
        assert!(
            !urls.iter().any(|u| u.ends_with("/news/australia-")),
            "should not contain truncated /news/australia- (old regex bug); urls = {urls:#?}"
        );

        // Regression guard #2: absolute-URL href must also be captured.
        // Previously the regex required href to start with /news/ directly.
        assert!(
            urls.contains(&"https://www.anthropic.com/news/announcing-our-updated-responsible-scaling-policy".to_string()),
            "missing announcing-our-updated-responsible-scaling-policy (absolute-href slug); urls = {urls:#?}"
        );

        // Sanity: known stable article should still parse.
        assert!(
            urls.contains(&"https://www.anthropic.com/news/claude-sonnet-4-6".to_string()),
            "missing baseline slug claude-sonnet-4-6; urls = {urls:#?}"
        );
    }

    #[test]
    fn deduplicates_absolute_and_relative_hrefs_for_same_article() {
        let html = r#"
            <a href="/news/foo-bar">rel</a>
            <a href="https://www.anthropic.com/news/foo-bar">abs</a>
            <a href="https://www.anthropic.com/news/baz-QUX">other</a>
        "#;
        let urls = NewsMonitorHook::parse_anthropic_slugs(html);
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0], "https://www.anthropic.com/news/foo-bar");
        assert_eq!(urls[1], "https://www.anthropic.com/news/baz-QUX");
    }
}
