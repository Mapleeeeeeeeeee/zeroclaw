use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{Datelike, NaiveDate, NaiveTime};
use parking_lot::Mutex;
use rusqlite::Connection;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::hooks::traits::HookHandler;
use crate::providers::Provider;

fn default_water_daily_goal() -> i32 { 2000 }
fn default_water_per_sip() -> i32 { 30 }
fn default_water_min_sips() -> u32 { 5 }
fn default_water_max_sips() -> u32 { 8 }
fn default_water_work_start() -> String { "01:30".to_string() }
fn default_water_work_end() -> String { "10:30".to_string() }
fn default_water_lunch_start() -> String { "04:30".to_string() }
fn default_water_lunch_end() -> String { "05:30".to_string() }
fn default_water_interval_min() -> u32 { 20 }
fn default_water_interval_max() -> u32 { 35 }
fn default_water_snooze() -> u64 { 180 }
fn default_water_db_path() -> String { "/home/azureuser/.water-reminder/water.db".to_string() }

/// Configuration for the water-reminder builtin hook.
///
/// When enabled, sends periodic hydration reminders via Telegram and tracks
/// daily intake using a local SQLite database.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WaterReminderConfig {
    /// Enable the water-reminder hook. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Telegram chat ID to send reminders to.
    #[serde(default)]
    pub chat_id: String,
    /// Daily water intake goal in millilitres. Default: `2000`.
    #[serde(default = "default_water_daily_goal")]
    pub daily_goal_ml: i32,
    /// Millilitres per sip/drink unit. Default: `30`.
    #[serde(default = "default_water_per_sip")]
    pub per_drink_ml: i32,
    /// Minimum sips per reminder. Default: `5`.
    #[serde(default = "default_water_min_sips")]
    pub min_sips: u32,
    /// Maximum sips per reminder. Default: `8`.
    #[serde(default = "default_water_max_sips")]
    pub max_sips: u32,
    /// Work start time (HH:MM, UTC). Default: `"01:30"`.
    #[serde(default = "default_water_work_start")]
    pub work_start: String,
    /// Work end time (HH:MM, UTC). Default: `"10:30"`.
    #[serde(default = "default_water_work_end")]
    pub work_end: String,
    /// Lunch window start (HH:MM, UTC). Default: `"04:30"`.
    #[serde(default = "default_water_lunch_start")]
    pub lunch_start: String,
    /// Lunch window end (HH:MM, UTC). Default: `"05:30"`.
    #[serde(default = "default_water_lunch_end")]
    pub lunch_end: String,
    /// Minimum interval between reminders (minutes). Default: `20`.
    #[serde(default = "default_water_interval_min")]
    pub interval_min_minutes: u32,
    /// Maximum interval between reminders (minutes). Default: `35`.
    #[serde(default = "default_water_interval_max")]
    pub interval_max_minutes: u32,
    /// Seconds to wait before sending a snooze reminder. Default: `180`.
    #[serde(default = "default_water_snooze")]
    pub snooze_wait_seconds: u64,
    /// Path to the SQLite database file. Default: `~/.water-reminder/water.db`.
    #[serde(default = "default_water_db_path")]
    pub db_path: String,
}

impl Default for WaterReminderConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            chat_id: String::new(),
            daily_goal_ml: default_water_daily_goal(),
            per_drink_ml: default_water_per_sip(),
            min_sips: default_water_min_sips(),
            max_sips: default_water_max_sips(),
            work_start: default_water_work_start(),
            work_end: default_water_work_end(),
            lunch_start: default_water_lunch_start(),
            lunch_end: default_water_lunch_end(),
            interval_min_minutes: default_water_interval_min(),
            interval_max_minutes: default_water_interval_max(),
            snooze_wait_seconds: default_water_snooze(),
            db_path: default_water_db_path(),
        }
    }
}

impl WaterReminderConfig {
    pub fn parse_work_start(&self) -> Option<NaiveTime> { NaiveTime::parse_from_str(&self.work_start, "%H:%M").ok() }
    pub fn parse_work_end(&self) -> Option<NaiveTime> { NaiveTime::parse_from_str(&self.work_end, "%H:%M").ok() }
    pub fn parse_lunch_start(&self) -> Option<NaiveTime> { NaiveTime::parse_from_str(&self.lunch_start, "%H:%M").ok() }
    pub fn parse_lunch_end(&self) -> Option<NaiveTime> { NaiveTime::parse_from_str(&self.lunch_end, "%H:%M").ok() }
    pub fn total_drinks_goal(&self) -> i32 {
        if self.per_drink_ml <= 0 { return 0; }
        (self.daily_goal_ml + self.per_drink_ml - 1) / self.per_drink_ml
    }
}

#[derive(Debug, Clone)]
pub struct ReminderState {
    pub date: String,
    pub drink_count: i32,
    pub goal_ml: i32,
    pub per_drink_ml: i32,
    pub current_streak: i32,
    pub last_goal_date: Option<String>,
}

impl ReminderState {
    pub fn total_drinks_goal(&self) -> i32 {
        if self.per_drink_ml <= 0 { return 0; }
        (self.goal_ml + self.per_drink_ml - 1) / self.per_drink_ml
    }
    pub fn ml_consumed(&self) -> i32 { self.drink_count * self.per_drink_ml }
    pub fn goal_reached(&self) -> bool { self.ml_consumed() >= self.goal_ml }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TimeContext { MorningStart, Midday, Afternoon, Closing, Outside, }

impl TimeContext {
    pub fn from_time(t: &NaiveTime) -> Self {
        let t9_30 = NaiveTime::from_hms_opt(1, 30, 0).unwrap();   // 09:30 CST
        let t11_00 = NaiveTime::from_hms_opt(3, 0, 0).unwrap();  // 11:00 CST
        let t14_00 = NaiveTime::from_hms_opt(6, 0, 0).unwrap();  // 14:00 CST
        let t17_00 = NaiveTime::from_hms_opt(9, 0, 0).unwrap();  // 17:00 CST
        let t18_30 = NaiveTime::from_hms_opt(10, 30, 0).unwrap();// 18:30 CST
        if *t >= t9_30 && *t < t11_00 { TimeContext::MorningStart }
        else if *t >= t11_00 && *t < t14_00 { TimeContext::Midday }
        else if *t >= t14_00 && *t < t17_00 { TimeContext::Afternoon }
        else if *t >= t17_00 && *t < t18_30 { TimeContext::Closing }
        else { TimeContext::Outside }
    }
    pub fn label(&self) -> &'static str {
        match self {
            TimeContext::MorningStart => "早上開始",
            TimeContext::Midday => "午間",
            TimeContext::Afternoon => "下午",
            TimeContext::Closing => "下班前",
            TimeContext::Outside => "非工作時間",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WaterReminderError {
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("Config error: {0}")]
    Config(String),
}

pub struct WaterReminderHook {
    pub config: WaterReminderConfig,
    pub db: Arc<Mutex<Connection>>,
    pub http_client: reqwest::Client,
    pub provider: Arc<dyn Provider>,
    pub model: String,
    pub identity: String,
}

impl WaterReminderHook {
    pub fn new(config: WaterReminderConfig, provider: Arc<dyn Provider>, model: String) -> Result<Self, WaterReminderError> {
        if config.db_path != ":memory:" {
            if let Some(parent) = std::path::Path::new(&config.db_path).parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        WaterReminderError::Config(format!("Cannot create db dir: {e}"))
                    })?;
                }
            }
        }
        let conn = Connection::open(&config.db_path)?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS daily_log (
                date TEXT NOT NULL, drink_count INTEGER NOT NULL DEFAULT 0,
                goal_ml INTEGER NOT NULL DEFAULT 2000, per_drink_ml INTEGER NOT NULL DEFAULT 30,
                PRIMARY KEY (date));
            CREATE TABLE IF NOT EXISTS scheduler_state (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS streak (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                current_streak INTEGER NOT NULL DEFAULT 0, last_goal_date TEXT);
            INSERT OR IGNORE INTO streak (id, current_streak) VALUES (1, 0);")?;
        let identity = std::fs::read_to_string("/home/azureuser/.zeroclaw/workspace/IDENTITY.md")
            .unwrap_or_default();
        Ok(Self { db: Arc::new(Mutex::new(conn)), config, http_client: reqwest::Client::new(), provider, model, identity })
    }

    pub fn get_state(&self, date: &str) -> Result<ReminderState, WaterReminderError> {
        let db = self.db.lock();
        db.execute("INSERT OR IGNORE INTO daily_log (date, drink_count, goal_ml, per_drink_ml) VALUES (?1, 0, ?2, ?3)",
            rusqlite::params![date, self.config.daily_goal_ml, self.config.per_drink_ml])?;
        let (drink_count, goal_ml, per_drink_ml): (i32, i32, i32) = db.query_row(
            "SELECT drink_count, goal_ml, per_drink_ml FROM daily_log WHERE date = ?1",
            rusqlite::params![date], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        let (current_streak, last_goal_date): (i32, Option<String>) = db.query_row(
            "SELECT current_streak, last_goal_date FROM streak WHERE id = 1",
            [], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(ReminderState { date: date.to_string(), drink_count, goal_ml, per_drink_ml, current_streak, last_goal_date })
    }

    pub fn record_drink(&self, date: &str, sips: i32) -> Result<ReminderState, WaterReminderError> {
        let db = self.db.lock();
        db.execute(
            "INSERT INTO daily_log (date, drink_count, goal_ml, per_drink_ml) VALUES (?1, ?4, ?2, ?3)
             ON CONFLICT(date) DO UPDATE SET drink_count = drink_count + ?4",
            rusqlite::params![date, self.config.daily_goal_ml, self.config.per_drink_ml, sips])?;
        let (drink_count, goal_ml, per_drink_ml): (i32, i32, i32) = db.query_row(
            "SELECT drink_count, goal_ml, per_drink_ml FROM daily_log WHERE date = ?1",
            rusqlite::params![date], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        let (current_streak, last_goal_date): (i32, Option<String>) = db.query_row(
            "SELECT current_streak, last_goal_date FROM streak WHERE id = 1",
            [], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(ReminderState { date: date.to_string(), drink_count, goal_ml, per_drink_ml, current_streak, last_goal_date })
    }

    pub fn schedule_next(&self, next_epoch: u64) -> Result<(), WaterReminderError> {
        let db = self.db.lock();
        db.execute("INSERT INTO scheduler_state (key, value) VALUES ('next_reminder', ?1)
             ON CONFLICT(key) DO UPDATE SET value = ?1", rusqlite::params![next_epoch.to_string()])?;
        Ok(())
    }

    pub fn finalize_day(&self, date: &str) -> Result<(), WaterReminderError> {
        let db = self.db.lock();
        let result: Option<(i32, i32, i32)> = db.query_row(
            "SELECT drink_count, goal_ml, per_drink_ml FROM daily_log WHERE date = ?1",
            rusqlite::params![date], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).ok();
        if let Some((drinks, goal_ml, per_drink_ml)) = result {
            if drinks * per_drink_ml >= goal_ml {
                db.execute("UPDATE streak SET current_streak = current_streak + 1, last_goal_date = ?1 WHERE id = 1",
                    rusqlite::params![date])?;
            } else {
                db.execute("UPDATE streak SET current_streak = 0 WHERE id = 1", [])?;
            }
        }
        // Clear pending state so it does not carry over to next day
        db.execute("DELETE FROM scheduler_state WHERE key IN (pending_message_id, pending_sent_at, snooze_sent, original_message_id)", [])?;
        Ok(())
    }

    pub fn get_scheduler_value(&self, key: &str) -> Result<Option<String>, WaterReminderError> {
        let db = self.db.lock();
        Ok(db.query_row("SELECT value FROM scheduler_state WHERE key = ?1", rusqlite::params![key], |row| row.get(0)).ok())
    }

    pub fn set_scheduler_value(&self, key: &str, value: &str) -> Result<(), WaterReminderError> {
        let db = self.db.lock();
        db.execute("INSERT INTO scheduler_state (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = ?2",
            rusqlite::params![key, value])?;
        Ok(())
    }

    pub fn clear_pending(&self, key: &str) -> Result<(), WaterReminderError> {
        let db = self.db.lock();
        db.execute("DELETE FROM scheduler_state WHERE key = ?1", rusqlite::params![key])?;
        Ok(())
    }

    const LEAVE_DATE_KEY: &'static str = "leave_date";

    /// Call provider directly to generate AI text. Returns fallback on failure.
    pub async fn call_ai(&self, prompt: &str) -> String {
        match self.provider.chat_with_system(Some(&self.identity), prompt, &self.model, 0.9).await {
            Ok(response) => parse_ai_response(&response),
            Err(e) => {
                tracing::warn!("water_reminder call_ai error: {e}");
                "主子，該喝水了 💧".to_string()
            }
        }
    }

    fn is_on_leave_today(&self) -> bool {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        match self.get_scheduler_value(Self::LEAVE_DATE_KEY) {
            Ok(Some(date_str)) if !date_str.is_empty() => date_str == today,
            _ => false, // fail-open: DB error or no leave → not on leave
        }
    }

    fn is_after_work_end(&self) -> bool {
        let now_time = chrono::Local::now().time();
        match self.config.parse_work_end() {
            Some(end) => now_time >= end,
            None => false,
        }
    }

    async fn handle_leave_command(&self) -> String {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let state = self.get_state(&today).ok();
        let progress = match &state {
            Some(s) => format!("目前進度：已喝{}口（{}ml / {}ml），連續天數{}天。", s.drink_count, s.ml_consumed(), s.goal_ml, s.current_streak),
            None => "今日尚未開始喝水。".to_string(),
        };

        if self.is_on_leave_today() {
            let prompt = format!("你是小允子。主子剛說了「請假」但今天已經請過假了。{}用甄嬛傳風格回覆，提醒主子已請過假正在休息中，50字以內，帶emoji。只輸出回覆的話。", progress);
            return self.call_ai(&prompt).await;
        }
        if self.is_after_work_end() {
            let prompt = format!("你是小允子。主子說「請假」但今天提醒時段已經結束了。{}用甄嬛傳風格回覆主子今日提醒已結束，明日再說，50字以內，帶emoji。只輸出回覆的話。", progress);
            return self.call_ai(&prompt).await;
        }
        match self.set_scheduler_value(Self::LEAVE_DATE_KEY, &today) {
            Ok(_) => {
                tracing::info!("🚰 Leave set for {today}");
                let ai_reply = self.call_ai("你是小允子。主子今天請假了。用甄嬛傳風格回覆確認請假，告知今日不再打擾、明早九點半恢復，30字以內，帶emoji。只輸出回覆的話。").await;
                format!("{}\n\n📊 {}", ai_reply, progress)
            }
            Err(e) => {
                tracing::error!("🚰 Failed to set leave_date: {e}");
                "奴才辦事不力，請主子稍後再試 😢".to_string()
            }
        }
    }

    async fn handle_unleave_command(&self) -> String {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let state = self.get_state(&today).ok();
        let progress = match &state {
            Some(s) => format!("目前進度：已喝{}口（{}ml / {}ml），連續天數{}天。", s.drink_count, s.ml_consumed(), s.goal_ml, s.current_streak),
            None => "今日尚未開始喝水。".to_string(),
        };

        if !self.is_on_leave_today() {
            let prompt = format!("你是小允子。主子說「銷假」但今天並未請假。{}用甄嬛傳風格回覆主子並未請假，50字以內，帶emoji。只輸出回覆的話。", progress);
            return self.call_ai(&prompt).await;
        }
        match self.clear_pending(Self::LEAVE_DATE_KEY) {
            Ok(_) => {
                tracing::info!("🚰 Leave cleared");
                let ai_reply = self.call_ai("你是小允子。主子銷假回來了。用甄嬛傳風格歡迎主子回來並表示馬上備水，30字以內，帶emoji。只輸出回覆的話。").await;
                format!("{}\n\n📊 {}", ai_reply, progress)
            }
            Err(e) => {
                tracing::error!("🚰 Failed to clear leave_date: {e}");
                "奴才辦事不力，請主子稍後再試 😢".to_string()
            }
        }
    }

    async fn send_telegram_text(&self, text: &str) -> Result<(), WaterReminderError> {
        let body = serde_json::json!({
            "chat_id": self.config.chat_id,
            "text": text,
        });
        let _ = self.http_client
            .post(format!("https://api.telegram.org/bot{}/sendMessage", self.tg_token()))
            .json(&body)
            .send()
            .await;
        Ok(())
    }

    /// Send a water reminder with inline keyboard
    pub async fn send_reminder(&self, is_snooze: bool) -> Result<(), WaterReminderError> {
        let now = chrono::Local::now();
        let date = now.format("%Y-%m-%d").to_string();
        let state = self.get_state(&date)?;

        // For snooze: reuse original sip count. For new reminder: generate random.
        let sips = if is_snooze {
            self.get_scheduler_value("pending_sips")
                .ok()
                .flatten()
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or_else(|| {
                    use rand::RngExt;
                    rand::rng().random_range(self.config.min_sips..=self.config.max_sips)
                })
        } else {
            use rand::RngExt;
            let mut rng = rand::rng();
            rng.random_range(self.config.min_sips..=self.config.max_sips)
        };

        let prompt = build_ai_prompt(&state, is_snooze, sips);
        let ai_text = self.call_ai(&prompt).await;

        let keyboard = serde_json::json!({
            "inline_keyboard": [[{
                "text": format!("✅ 已喝 {} 口", sips),
                "callback_data": format!("water:confirm:{}", sips)
            }]]
        });

        let body = serde_json::json!({
            "chat_id": self.config.chat_id,
            "text": ai_text,
            "reply_markup": keyboard
        });

        let resp = self.http_client
            .post(format!("https://api.telegram.org/bot{}/sendMessage", self.tg_token()))
            .json(&body)
            .send()
            .await
            .map_err(|e| WaterReminderError::Config(format!("TG send error: {e}")))?;

        // Extract message_id from response
        if let Ok(data) = resp.json::<serde_json::Value>().await {
            if let Some(msg_id) = data.pointer("/result/message_id").and_then(|v| v.as_i64()) {
                self.set_scheduler_value("pending_message_id", &msg_id.to_string())?;
                let sent_at = chrono::Utc::now().timestamp();
                self.set_scheduler_value("pending_sent_at", &sent_at.to_string())?;
                self.set_scheduler_value("snooze_sent", if is_snooze { "true" } else { "false" })?;
                if !is_snooze {
                    self.set_scheduler_value("pending_sips", &sips.to_string())?;
                }
            }
        }

        Ok(())
    }

    /// Send confirmation after drink is recorded (edit the original message)
    pub async fn send_confirmation(&self, chat_id: &str, message_id: i64, sips: i32) -> Result<(), WaterReminderError> {
        let now = chrono::Local::now();
        let date = now.format("%Y-%m-%d").to_string();
        let state = self.get_state(&date)?;
        let bar = build_progress_bar(state.drink_count, state.total_drinks_goal());
        let text = format!("✅ 已記錄 {}口！{bar} {}/{}ml（共{}口）",
            sips, state.ml_consumed(), state.goal_ml, state.drink_count);

        let body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": text,
        });

        let _ = self.http_client
            .post(format!("https://api.telegram.org/bot{}/editMessageText", self.tg_token()))
            .json(&body)
            .send()
            .await;

        Ok(())
    }

    /// Send daily report at 18:30
    pub async fn send_daily_report(&self) -> Result<(), WaterReminderError> {
        let now = chrono::Local::now();
        let date = now.format("%Y-%m-%d").to_string();

        self.finalize_day(&date)?;
        let state = self.get_state(&date)?;
        let goal_reached = state.goal_reached();

        let prompt = build_ai_report_prompt(&state, goal_reached);
        let ai_text = self.call_ai(&prompt).await;

        let bar = build_progress_bar(state.drink_count, state.total_drinks_goal());
        let streak_text = if state.current_streak > 0 {
            format!("🔥 連續 {} 天達標", state.current_streak)
        } else {
            "💪 明天再加油".to_string()
        };
        let text = format!("📊 今日喝水報告\n{bar} {}/{}ml\n{}\n\n{}",
            state.ml_consumed(), state.goal_ml, streak_text, ai_text);

        let body = serde_json::json!({
            "chat_id": self.config.chat_id,
            "text": text,
        });

        let _ = self.http_client
            .post(format!("https://api.telegram.org/bot{}/sendMessage", self.tg_token()))
            .json(&body)
            .send()
            .await;

        self.set_scheduler_value("report_sent_today", &date)?;
        Ok(())
    }

    /// Background tick — runs every 60s. Handles:
    /// 1. Send reminder if next_reminder time is reached
    /// 2. Snooze check (3 min after pending, if not confirmed)
    /// 3. Daily report at 18:30
    /// 4. Lunch reminder (~13:00)
    pub async fn background_tick(&self) -> Result<(), WaterReminderError> {
        let now = chrono::Local::now();
        let now_time = now.time();
        let now_date = now.date_naive();
        let date_str = now.format("%Y-%m-%d").to_string();

        // Leave guard — skip all reminders if on leave today
        if self.is_on_leave_today() {
            return Ok(());
        }

        // Skip weekends
        if !is_weekday(&now_date) {
            return Ok(());
        }

        // Skip outside work hours (except for 18:30 report)
        let in_work_hours = is_work_time(&now_time, &self.config.work_start, &self.config.work_end);

        // --- Daily report at 18:30 ---
        let report_time = chrono::NaiveTime::from_hms_opt(10, 30, 0).unwrap(); // 18:30 CST
        if now_time >= report_time && now_time < chrono::NaiveTime::from_hms_opt(10, 35, 0).unwrap() { // 18:35 CST
            let report_sent = self.get_scheduler_value("report_sent_today")?.unwrap_or_default();
            if report_sent != date_str {
                self.send_daily_report().await?;
            }
        }

        if !in_work_hours {
            return Ok(());
        }

        // --- Snooze check ---
        if let Some(pending_sent_at) = self.get_scheduler_value("pending_sent_at")? {
            let sent_epoch: i64 = pending_sent_at.parse().unwrap_or(0);
            let elapsed = chrono::Utc::now().timestamp() - sent_epoch;
            let snooze_sent = self.get_scheduler_value("snooze_sent")?.unwrap_or_default();

            if elapsed >= self.config.snooze_wait_seconds as i64 {
                if snooze_sent != "true" {
                    // Save original message_id before snooze sends a new one
                    if let Some(orig_id) = self.get_scheduler_value("pending_message_id")? {
                        self.set_scheduler_value("original_message_id", &orig_id)?;
                    }
                    // Send snooze reminder
                    tracing::info!("🚰 Sending snooze reminder");
                    self.send_reminder(true).await?;
                } else {
                    // Already snoozed once, abandon and schedule next
                    tracing::info!("🚰 Snooze abandoned, scheduling next reminder");
                    self.clear_pending("pending_message_id")?;
                    self.clear_pending("pending_sent_at")?;
                    self.clear_pending("snooze_sent")?;
                    self.clear_pending("original_message_id")?;
                    // Schedule next reminder so it doesn't immediately re-trigger
                    let interval = next_interval_seconds(
                        self.config.interval_min_minutes,
                        self.config.interval_max_minutes,
                    );
                    let next = chrono::Utc::now().timestamp() as u64 + interval as u64;
                    self.schedule_next(next)?;
                }
            }
            return Ok(()); // Don't send new reminder while pending
        }

        // --- Lunch window: send one reminder around 13:00 ---
        if is_lunch_window(&now_time, &self.config.lunch_start, &self.config.lunch_end) {
            let lunch_midpoint = chrono::NaiveTime::from_hms_opt(5, 0, 0).unwrap(); // 13:00 CST
            let lunch_sent = self.get_scheduler_value("lunch_sent_today")?.unwrap_or_default();
            if now_time >= lunch_midpoint && lunch_sent != date_str {
                self.send_reminder(false).await?;
                self.set_scheduler_value("lunch_sent_today", &date_str)?;
                // Schedule next after lunch
                let next = chrono::Utc::now().timestamp() as u64
                    + next_interval_seconds(self.config.interval_min_minutes, self.config.interval_max_minutes) as u64;
                self.schedule_next(next)?;
            }
            return Ok(()); // During lunch, only the midpoint reminder
        }

        // --- First reminder of the day: fill water bottle ---
        let fill_sent = self.get_scheduler_value("fill_bottle_today")?.unwrap_or_default();
        if fill_sent != date_str {
            // New day started — clean up stale processed_msg_* entries to prevent DB bloat
            {
                let db = self.db.lock();
                if let Err(e) = db.execute("DELETE FROM scheduler_state WHERE key LIKE 'processed_msg_%'", []) {
                    tracing::warn!("water_reminder: failed to clean processed_msg entries: {e}");
                }
            }
            tracing::info!("🚰 Sending fill-bottle reminder for today");
            self.send_fill_bottle_reminder().await?;
            self.set_scheduler_value("fill_bottle_today", &date_str)?;
            // Schedule first drinking reminder 5-10 minutes later
            let delay = {
                use rand::RngExt;
                let mut rng = rand::rng();
                rng.random_range(5u32..=10u32) * 60
            };
            let next = chrono::Utc::now().timestamp() as u64 + delay as u64;
            self.schedule_next(next)?;
            tracing::info!("🚰 First drinking reminder scheduled in {} seconds", delay);
            return Ok(());
        }

        // --- Normal reminder check ---
        let next_reminder = self.get_scheduler_value("next_reminder")?;
        let now_epoch = chrono::Utc::now().timestamp() as u64;

        match next_reminder {
            Some(next_str) => {
                let next_epoch: u64 = next_str.parse().unwrap_or(0);
                if now_epoch >= next_epoch {
                    // Time to remind!
                    self.send_reminder(false).await?;
                    let interval = next_interval_seconds(
                        self.config.interval_min_minutes,
                        self.config.interval_max_minutes,
                    );
                    self.schedule_next(now_epoch + interval as u64)?;
                }
            }
            None => {
                // No next_reminder set — schedule one (safety net / first start)
                let interval = next_interval_seconds(
                    self.config.interval_min_minutes,
                    self.config.interval_max_minutes,
                );
                self.schedule_next(now_epoch + interval as u64)?;
                tracing::info!("🚰 Scheduled first reminder in {} seconds", interval);
            }
        }

        Ok(())
    }

    /// Send a "fill water bottle" reminder at start of work day (no inline keyboard)
    pub async fn send_fill_bottle_reminder(&self) -> Result<(), WaterReminderError> {
        let prompt = "你是小允子。現在是上班時間開始，主子剛到辦公室。用甄嬛傳風格提醒主子先去裝水，50字以內，帶一個emoji。只輸出提醒的話。";
        let ai_text = match self.provider.chat_with_system(Some(&self.identity), prompt, &self.model, 0.9).await {
            Ok(response) => parse_ai_response(&response),
            Err(_) => "主子，先去裝杯水吧～奴才等您回來 🚰".to_string(),
        };

        let body = serde_json::json!({
            "chat_id": self.config.chat_id,
            "text": ai_text,
        });

        let _ = self.http_client
            .post(format!("https://api.telegram.org/bot{}/sendMessage", self.tg_token()))
            .json(&body)
            .send()
            .await;

        Ok(())
    }

    /// Helper: get TG bot token from environment or config
    fn tg_token(&self) -> String {
        // Read from ZeroClaw config - the token is in [channels_config.telegram] or we use the known token
        std::env::var("TG_BOT_TOKEN").unwrap_or_else(|_| "<REDACTED>".to_string())
    }
}

#[async_trait]
impl HookHandler for WaterReminderHook {
    fn name(&self) -> &str { "water_reminder" }

    async fn on_gateway_start(&self, _host: &str, _port: u16) {
        if !self.config.enabled {
            return;
        }
        tracing::info!("🚰 WaterReminderHook registered (goal: {}ml, interval: {}-{}min)",
            self.config.daily_goal_ml, self.config.interval_min_minutes, self.config.interval_max_minutes);

        // Spawn background tick task (60s interval)
        let config = self.config.clone();
        let db = Arc::clone(&self.db);
        let http_client = self.http_client.clone();
        let provider = Arc::clone(&self.provider);
        let model = self.model.clone();
        let identity = self.identity.clone();

        tokio::spawn(async move {
            let hook = WaterReminderHook { config, db, http_client, provider, model, identity };
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                if let Err(e) = hook.background_tick().await {
                    tracing::warn!("water_reminder tick error: {e}");
                }
            }
        });
    }

    async fn on_callback_query(&self, callback_data: &str, chat_id: &str, message_id: i64) {
        if !callback_data.starts_with("water:") {
            return;
        }

        if callback_data.starts_with("water:confirm") {
            // Parse sips: "water:confirm:6" → 6
            let sips: i32 = callback_data.split(':').nth(2)
                .and_then(|s| s.parse().ok())
                .unwrap_or(5); // default 5 sips if parse fails

            // Per-message dedup: check if this specific message_id was already processed.
            // This handles the case where snooze sends a new message (new message_id) —
            // old buttons remain pressable but are blocked by their own processed_msg entry.
            let processed_key = format!("processed_msg_{}", message_id);
            let already_processed = self.get_scheduler_value(&processed_key).unwrap_or(None);
            if already_processed.is_some() {
                tracing::warn!("water_reminder: message_id={} already processed, ignoring duplicate press", message_id);
                return;
            }

            // Record sips (user pressed the button)
            let now = chrono::Local::now();
            let date = now.format("%Y-%m-%d").to_string();

            match self.record_drink(&date, sips) {
                Ok(state) => {
                    tracing::info!("🚰 Sips recorded: {} sips, total {}/{} ({}ml)",
                        sips, state.drink_count, state.total_drinks_goal(), state.ml_consumed());
                }
                Err(e) => {
                    tracing::warn!("water_reminder record_drink error: {e}");
                }
            }

            // Mark this message_id as processed (prevents duplicate presses on the same button)
            let _ = self.set_scheduler_value(&processed_key, "1");

            // Clear pending state if this message_id matches the current pending one (normal flow).
            // If the user pressed an old/snooze message button, pending_message_id may differ —
            // we still mark the old button processed but leave the new pending intact.
            let pending_id = self.get_scheduler_value("pending_message_id").unwrap_or(None);
            let pending_matches = pending_id
                .as_deref()
                .and_then(|s| s.parse::<i64>().ok())
                == Some(message_id);
            if pending_matches {
                let _ = self.clear_pending("pending_message_id");
                let _ = self.clear_pending("pending_sent_at");
                let _ = self.clear_pending("snooze_sent");
            }

            // Edit the original message with confirmation
            if let Err(e) = self.send_confirmation(chat_id, message_id, sips).await {
                tracing::warn!("water_reminder send_confirmation error: {e}");
            }

            // Also edit the OTHER message (original or snooze) so both show confirmation
            if let Ok(Some(orig_id_str)) = self.get_scheduler_value("original_message_id") {
                if let Ok(orig_id) = orig_id_str.parse::<i64>() {
                    if orig_id != message_id {
                        let _ = self.send_confirmation(chat_id, orig_id, sips).await;
                    }
                }
                let _ = self.clear_pending("original_message_id");
            }
            // If user pressed the ORIGINAL message button, also update the snooze message
            if !pending_matches {
                if let Some(snooze_id_str) = &pending_id {
                    if let Ok(snooze_id) = snooze_id_str.parse::<i64>() {
                        if snooze_id != message_id {
                            let _ = self.send_confirmation(chat_id, snooze_id, sips).await;
                        }
                    }
                    let _ = self.clear_pending("pending_message_id");
                    let _ = self.clear_pending("pending_sent_at");
                    let _ = self.clear_pending("snooze_sent");
                }
            }
        }
    }

    async fn on_message_received(&self, message: crate::channels::traits::ChannelMessage) -> crate::hooks::traits::HookResult<crate::channels::traits::ChannelMessage> {
        if !self.config.enabled {
            return crate::hooks::traits::HookResult::Continue(message);
        }

        let text = message.content.trim().to_string();

        if text == "請假" || text == "/leave" {
            let reply = self.handle_leave_command().await;
            if let Err(e) = self.send_telegram_text(&reply).await {
                tracing::error!("🚰 Failed to send leave reply: {e}");
            }
            return crate::hooks::traits::HookResult::Cancel("water_reminder: leave command handled".to_string());
        }

        if text == "銷假" || text == "/unleave" {
            let reply = self.handle_unleave_command().await;
            if let Err(e) = self.send_telegram_text(&reply).await {
                tracing::error!("🚰 Failed to send unleave reply: {e}");
            }
            return crate::hooks::traits::HookResult::Cancel("water_reminder: unleave command handled".to_string());
        }

        crate::hooks::traits::HookResult::Continue(message)
    }
}

// Pure business logic functions

pub fn is_work_time(now: &NaiveTime, work_start: &str, work_end: &str) -> bool {
    let Ok(start) = NaiveTime::parse_from_str(work_start, "%H:%M") else { return false; };
    let Ok(end) = NaiveTime::parse_from_str(work_end, "%H:%M") else { return false; };
    *now >= start && *now < end
}

pub fn is_lunch_window(now: &NaiveTime, lunch_start: &str, lunch_end: &str) -> bool {
    let Ok(start) = NaiveTime::parse_from_str(lunch_start, "%H:%M") else { return false; };
    let Ok(end) = NaiveTime::parse_from_str(lunch_end, "%H:%M") else { return false; };
    *now >= start && *now < end
}

pub fn is_weekday(date: &NaiveDate) -> bool {
    use chrono::Weekday;
    !matches!(date.weekday(), Weekday::Sat | Weekday::Sun)
}

pub fn next_interval_seconds(min_minutes: u32, max_minutes: u32) -> u32 {
    use rand::RngExt;
    if min_minutes >= max_minutes { return min_minutes * 60; }
    let mut rng = rand::rng();
    rng.random_range(min_minutes..=max_minutes) * 60
}

pub fn build_progress_bar(drinks: i32, total_drinks: i32) -> String {
    const FULL: char = '\u{2588}';
    const EMPTY: char = '\u{2591}';
    if total_drinks <= 0 { return std::iter::repeat(EMPTY).take(10).collect(); }
    let filled = ((drinks as f64 / total_drinks as f64) * 10.0).round() as usize;
    let filled = filled.min(10);
    let f: String = std::iter::repeat(FULL).take(filled).collect();
    let e: String = std::iter::repeat(EMPTY).take(10 - filled).collect();
    format!("{}{}", f, e)
}

pub fn build_ai_prompt(state: &ReminderState, is_snooze: bool, sips: u32) -> String {
    let total = state.total_drinks_goal();
    let bar = build_progress_bar(state.drink_count, total);
    let ctx = TimeContext::from_time(&chrono::Local::now().time());
    let snooze_note = if is_snooze { "（這是一次提醒後 snooze 的再次提醒）" } else { "" };
    let sip_ml = sips * 30;
    format!("你是小允子，一個體貼又有點俳皮的喝水提醒助理，專門服務你的主子。\n現在是{time_label}時段。{snooze_note}\n主子今天的喝水狀況：\n- 已喝：{drinks} 口（{ml_consumed} ml），目標 {goal_ml} ml\n- 進度：{bar}（{drinks}/{total} 口）\n- 連續達標天數：{streak} 天\n\n請提醒主子喝 {sips} 口水（約 {sip_ml}ml）。直接在提醒中說「喝{sips}口」。\n用溫柔撒婬但不誧張的方式提醒，50字以內，不要換行，可以用一個表情符號結尾。\n如果 snooze 的話可以加一點「主子你又忘啦」的俳皮感。\n只輸出提醒的話，不要解釋。",
        time_label = ctx.label(), snooze_note = snooze_note,
        drinks = state.drink_count, ml_consumed = state.ml_consumed(),
        goal_ml = state.goal_ml, bar = bar, total = total, streak = state.current_streak,
        sips = sips, sip_ml = sip_ml)
}

pub fn build_ai_report_prompt(state: &ReminderState, goal_reached: bool) -> String {
    let total = state.total_drinks_goal();
    let bar = build_progress_bar(state.drink_count, total);
    let result_note = if goal_reached {
        format!("主子今天達標了！連續達標 {} 天🎉", state.current_streak)
    } else {
        format!("主子今天喝了 {} 口，還差 {} 口才達標。", state.drink_count, (total - state.drink_count).max(0))
    };
    format!("你是小允子，喝水提醒助理。今天的喝水報告如下：\n- 日期：{date}\n- 喝水次數：{drinks} 口（{ml} ml），目標 {goal_ml} ml\n- 進度：{bar}\n- {result_note}\n- 連續達標：{streak} 天\n\n請用輕鬆溫暖的語氣寫一個每日喝水總結報告，100字以內。如果達標請給予鼓勵；如果沒達標請溫柔地說明。只輸出報告內容，不要解釋。",
        date = state.date, drinks = state.drink_count, ml = state.ml_consumed(),
        goal_ml = state.goal_ml, bar = bar, result_note = result_note, streak = state.current_streak)
}

pub fn parse_ai_response(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() { "主子，該喝水了 💧".to_string() } else { trimmed.to_string() }
}

pub fn register(
    runner: &mut crate::hooks::HookRunner,
    config: &crate::config::schema::Config,
    provider: &std::sync::Arc<dyn crate::providers::Provider>,
    model: &str,
) {
    let Some(value) = config.hooks.builtin.extra.get("water_reminder") else { return; };
    let wr_config: WaterReminderConfig = match value.clone().try_into() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("water_reminder: invalid config: {e}");
            return;
        }
    };
    if !wr_config.enabled { return; }
    match WaterReminderHook::new(wr_config, Arc::clone(provider), model.to_string()) {
        Ok(hook) => {
            runner.register(Box::new(hook));
            tracing::info!("🚰 Water reminder hook registered");
        }
        Err(e) => tracing::warn!("Failed to initialize water reminder: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use chrono::NaiveDate;

    fn t(h: u32, m: u32) -> NaiveTime { NaiveTime::from_hms_opt(h, m, 0).unwrap() }
    fn d(y: i32, mo: u32, day: u32) -> NaiveDate { NaiveDate::from_ymd_opt(y, mo, day).unwrap() }
    struct TestProvider;
    #[async_trait::async_trait]
    impl crate::providers::Provider for TestProvider {
        async fn chat_with_system(&self, _sys: Option<&str>, _msg: &str, _model: &str, _temp: f64) -> anyhow::Result<String> {
            Ok("test response".to_string())
        }
    }

    fn hook() -> WaterReminderHook {
        WaterReminderHook::new(WaterReminderConfig {
            enabled: true, chat_id: "t".to_string(), daily_goal_ml: 2000, per_drink_ml: 30,
            min_sips: 5, max_sips: 8,
            work_start: "01:30".to_string(), work_end: "10:30".to_string(),
            lunch_start: "04:30".to_string(), lunch_end: "05:30".to_string(),
            interval_min_minutes: 30, interval_max_minutes: 50, snooze_wait_seconds: 180,
            db_path: ":memory:".to_string(),
        }, Arc::new(TestProvider), "test-model".to_string()).unwrap()
    }
    fn st(dc: i32) -> ReminderState { ReminderState { date: "2025-03-17".to_string(), drink_count: dc, goal_ml: 2000, per_drink_ml: 30, current_streak: 0, last_goal_date: None } }

    #[test] fn work_time_inside() { assert!(is_work_time(&t(2,0),"01:30","10:30")); }
    #[test] fn work_time_start_incl() { assert!(is_work_time(&t(1,30),"01:30","10:30")); }
    #[test] fn work_time_end_excl() { assert!(!is_work_time(&t(10,30),"01:30","10:30")); }
    #[test] fn work_time_before() { assert!(!is_work_time(&t(0,0),"01:30","10:30")); }
    #[test] fn work_time_after() { assert!(!is_work_time(&t(12,0),"01:30","10:30")); }
    #[test] fn work_time_bad_fmt() { assert!(!is_work_time(&t(2,0),"1:30am","10:30")); }
    #[test] fn lunch_inside() { assert!(is_lunch_window(&t(5,0),"04:30","05:30")); }
    #[test] fn lunch_start_incl() { assert!(is_lunch_window(&t(4,30),"04:30","05:30")); }
    #[test] fn lunch_end_excl() { assert!(!is_lunch_window(&t(5,30),"04:30","05:30")); }
    #[test] fn lunch_outside() { assert!(!is_lunch_window(&t(3,0),"04:30","05:30")); }
    #[test] fn monday_weekday() { assert!(is_weekday(&d(2025,3,17))); }
    #[test] fn friday_weekday() { assert!(is_weekday(&d(2025,3,21))); }
    #[test] fn saturday_not() { assert!(!is_weekday(&d(2025,3,22))); }
    #[test] fn sunday_not() { assert!(!is_weekday(&d(2025,3,23))); }
    #[test] fn interval_range() { for _ in 0..50 { let s = next_interval_seconds(30,50); assert!(s>=1800&&s<=3000); } }
    #[test] fn interval_equal() { assert_eq!(next_interval_seconds(45,45),2700); }
    #[test] fn interval_reversed() { assert_eq!(next_interval_seconds(60,30),3600); }
    #[test] fn bar_empty() { let b=build_progress_bar(0,10); assert_eq!(b.chars().count(),10); assert!(b.chars().all(|c|c=='\u{2591}')); }
    #[test] fn bar_full() { let b=build_progress_bar(10,10); assert!(b.chars().all(|c|c=='\u{2588}')); }
    #[test] fn bar_half() { let b=build_progress_bar(5,10); assert_eq!(b.chars().filter(|c|*c=='\u{2588}').count(),5); }
    #[test] fn bar_overflow() { let b=build_progress_bar(20,10); assert!(b.chars().all(|c|c=='\u{2588}')); }
    #[test] fn bar_zero_total() { let b=build_progress_bar(5,0); assert!(b.chars().all(|c|c=='\u{2591}')); }
    #[test] fn bar_always_10() { for i in 0..=15 { assert_eq!(build_progress_bar(i,13).chars().count(),10); } }
    #[test] fn ai_resp_trim() { assert_eq!(parse_ai_response("  hi  "),"hi"); }
    #[test] fn ai_resp_empty() { assert!(!parse_ai_response("").is_empty()); }
    #[test] fn ai_resp_ws() { assert!(!parse_ai_response("   ").is_empty()); }
    #[test] fn ai_prompt_has_data() { let p=build_ai_prompt(&st(3),false,6); assert!(p.contains("3")&&p.contains("2000")); }
    #[test] fn ai_prompt_has_sips() { let p=build_ai_prompt(&st(3),false,6); assert!(p.contains("6")); }
    #[test] fn ai_prompt_has_snooze() { assert!(build_ai_prompt(&st(1),true,5).contains("snooze")); }
    #[test] fn ai_prompt_no_snooze() { assert!(!build_ai_prompt(&st(1),false,5).contains("（這是一次提醒後 snooze 的再次提醒）")); }
    #[test] fn report_goal_has_streak() { let s=ReminderState{date:"x".to_string(),drink_count:14,goal_ml:2000,per_drink_ml:30,current_streak:3,last_goal_date:None}; assert!(build_ai_report_prompt(&s,true).contains("3")); }
    #[test] fn report_no_goal_has_date() { let s=ReminderState{date:"2025-03-17".to_string(),drink_count:5,goal_ml:2000,per_drink_ml:30,current_streak:0,last_goal_date:None}; assert!(build_ai_report_prompt(&s,false).contains("2025-03-17")); }
    #[test] fn tc_morning() { assert_eq!(TimeContext::from_time(&t(1,30)),TimeContext::MorningStart); assert_eq!(TimeContext::from_time(&t(2,59)),TimeContext::MorningStart); }
    #[test] fn tc_midday() { assert_eq!(TimeContext::from_time(&t(3,0)),TimeContext::Midday); }
    #[test] fn tc_afternoon() { assert_eq!(TimeContext::from_time(&t(6,0)),TimeContext::Afternoon); }
    #[test] fn tc_closing() { assert_eq!(TimeContext::from_time(&t(9,0)),TimeContext::Closing); assert_eq!(TimeContext::from_time(&t(10,29)),TimeContext::Closing); }
    #[test] fn tc_outside() { assert_eq!(TimeContext::from_time(&t(0,0)),TimeContext::Outside); assert_eq!(TimeContext::from_time(&t(10,30)),TimeContext::Outside); }
    #[test] fn ml_consumed() { assert_eq!(st(4).ml_consumed(),120); }
    #[test] fn goal_reached() { assert!(st(67).goal_reached()); assert!(!st(5).goal_reached()); }
    #[test] fn db_get_state() { let h=hook(); let s=h.get_state("2025-03-17").unwrap(); assert_eq!(s.drink_count,0); assert_eq!(s.goal_ml,2000); }
    #[test] fn db_record_drink() { let h=hook(); assert_eq!(h.record_drink("d",5).unwrap().drink_count,5); assert_eq!(h.record_drink("d",6).unwrap().drink_count,11); }
    #[test] fn db_schedule_next() { let h=hook(); h.schedule_next(1700).unwrap(); assert_eq!(h.get_scheduler_value("next_reminder").unwrap(),Some("1700".to_string())); }
    #[test] fn db_set_get() { let h=hook(); h.set_scheduler_value("k","v").unwrap(); assert_eq!(h.get_scheduler_value("k").unwrap(),Some("v".to_string())); }
    #[test] fn db_clear() { let h=hook(); h.set_scheduler_value("k","v").unwrap(); h.clear_pending("k").unwrap(); assert!(h.get_scheduler_value("k").unwrap().is_none()); }
    #[test] fn db_finalize_streak_up() { let h=hook(); for _ in 0..67 { h.record_drink("d",1).unwrap(); } h.finalize_day("d").unwrap(); assert_eq!(h.get_state("d").unwrap().current_streak,1); }
    #[test] fn db_finalize_streak_reset() { let h=hook(); { let db=h.db.lock(); db.execute("UPDATE streak SET current_streak=3 WHERE id=1",[]).unwrap(); } h.record_drink("d",1).unwrap(); h.finalize_day("d").unwrap(); assert_eq!(h.get_state("d").unwrap().current_streak,0); }
    #[test] fn sip_range_valid() { for _ in 0..50 { use rand::RngExt; let mut rng = rand::rng(); let s: u32 = rng.random_range(5u32..=8u32); assert!(s>=5&&s<=8); } }

    // Per-message dedup: processed_msg_{message_id} key guards against duplicate presses
    #[test] fn per_msg_dedup_first_press_allowed() {
        // No processed_msg entry → first press is allowed
        let h = hook();
        let message_id: i64 = 42;
        let key = format!("processed_msg_{}", message_id);
        let val = h.get_scheduler_value(&key).unwrap();
        assert!(val.is_none(), "no processed entry yet → press should be allowed");
    }

    #[test] fn per_msg_dedup_second_press_blocked() {
        // After first press marks the message as processed, second press is blocked
        let h = hook();
        let message_id: i64 = 42;
        let key = format!("processed_msg_{}", message_id);
        h.set_scheduler_value(&key, "1").unwrap();
        let val = h.get_scheduler_value(&key).unwrap();
        assert!(val.is_some(), "processed entry exists → duplicate press should be blocked");
    }

    #[test] fn per_msg_dedup_old_button_independent() {
        // Old message (id=10) can be independently marked processed without affecting new pending (id=20)
        let h = hook();
        h.set_scheduler_value("pending_message_id", "20").unwrap();
        h.set_scheduler_value("processed_msg_10", "1").unwrap();
        // old button blocked
        assert!(h.get_scheduler_value("processed_msg_10").unwrap().is_some());
        // new pending still intact
        assert_eq!(h.get_scheduler_value("pending_message_id").unwrap(), Some("20".to_string()));
        // new button not yet processed
        assert!(h.get_scheduler_value("processed_msg_20").unwrap().is_none());
    }

    #[test] fn per_msg_dedup_pending_cleared_when_matching() {
        // When the pressed message_id matches pending_message_id, pending state is cleared
        let h = hook();
        h.set_scheduler_value("pending_message_id", "7").unwrap();
        h.set_scheduler_value("pending_sent_at", "1000").unwrap();
        h.set_scheduler_value("snooze_sent", "false").unwrap();
        // Simulate the clear-pending logic for a matching message_id
        let pending_id = h.get_scheduler_value("pending_message_id").unwrap();
        let pending_matches = pending_id.as_deref().and_then(|s| s.parse::<i64>().ok()) == Some(7i64);
        assert!(pending_matches);
        h.clear_pending("pending_message_id").unwrap();
        h.clear_pending("pending_sent_at").unwrap();
        h.clear_pending("snooze_sent").unwrap();
        assert!(h.get_scheduler_value("pending_message_id").unwrap().is_none());
        assert!(h.get_scheduler_value("pending_sent_at").unwrap().is_none());
    }

    #[test] fn per_msg_dedup_pending_kept_when_not_matching() {
        // When the pressed message_id does NOT match pending_message_id (old/snooze button),
        // pending state for the new message must remain intact
        let h = hook();
        h.set_scheduler_value("pending_message_id", "20").unwrap(); // new message is pending
        // User presses old message button (id=10)
        let pending_id = h.get_scheduler_value("pending_message_id").unwrap();
        let pending_matches = pending_id.as_deref().and_then(|s| s.parse::<i64>().ok()) == Some(10i64);
        assert!(!pending_matches, "old button should NOT match current pending");
        // pending_message_id remains "20" — new message is still actionable
        assert_eq!(h.get_scheduler_value("pending_message_id").unwrap(), Some("20".to_string()));
    }

    #[test] fn processed_msg_cleanup_via_db() {
        // Verify that a DELETE LIKE query removes all processed_msg_* entries
        let h = hook();
        h.set_scheduler_value("processed_msg_1", "1").unwrap();
        h.set_scheduler_value("processed_msg_2", "1").unwrap();
        h.set_scheduler_value("next_reminder", "9999").unwrap(); // unrelated key, must survive
        {
            let db = h.db.lock();
            db.execute("DELETE FROM scheduler_state WHERE key LIKE 'processed_msg_%'", []).unwrap();
        }
        assert!(h.get_scheduler_value("processed_msg_1").unwrap().is_none());
        assert!(h.get_scheduler_value("processed_msg_2").unwrap().is_none());
        assert_eq!(h.get_scheduler_value("next_reminder").unwrap(), Some("9999".to_string()));
    }
}
