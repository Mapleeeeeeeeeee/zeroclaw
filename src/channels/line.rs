use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
    Router,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::{mpsc::Sender, Mutex};
use tracing::{debug, error, info, warn};

use super::traits::{Channel, ChannelMessage, SendMessage};

const LINE_REPLY_API: &str = "https://api.line.me/v2/bot/message/reply";
const LINE_PUSH_API: &str = "https://api.line.me/v2/bot/message/push";
const REPLY_TOKEN_TTL_SECS: u64 = 20;
const MAX_MESSAGE_LENGTH: usize = 5000;

#[derive(Debug, Deserialize)]
struct WebhookPayload {
    events: Vec<LineEvent>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum LineEvent {
    Message(MessageEvent),
    Follow(FollowEvent),
    Unfollow(UnfollowEvent),
    Join(JoinEvent),
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MessageEvent {
    reply_token: String,
    source: EventSource,
    timestamp: u64,
    message: LineMessage,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FollowEvent {
    reply_token: String,
    source: EventSource,
    timestamp: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UnfollowEvent {
    source: EventSource,
    #[allow(dead_code)]
    timestamp: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JoinEvent {
    reply_token: String,
    source: EventSource,
    timestamp: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventSource {
    #[serde(rename = "type")]
    source_type: String,
    user_id: Option<String>,
    group_id: Option<String>,
    room_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LineMessage {
    id: String,
    #[serde(rename = "type")]
    message_type: String,
    text: Option<String>,
}

#[derive(Debug, Serialize)]
struct ReplyRequest {
    #[serde(rename = "replyToken")]
    reply_token: String,
    messages: Vec<TextMessage>,
}

#[derive(Debug, Serialize)]
struct PushRequest {
    to: String,
    messages: Vec<TextMessage>,
}

#[derive(Debug, Serialize)]
struct MentionSubstitution {
    #[serde(rename = "type")]
    sub_type: String,
    mentionee: MentioneeRef,
}

#[derive(Debug, Serialize)]
struct MentioneeRef {
    #[serde(rename = "type")]
    mentionee_type: String,
    #[serde(rename = "userId", skip_serializing_if = "Option::is_none")]
    user_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum SubstitutionValue {
    Mention(MentionSubstitution),
}

#[derive(Debug, Serialize)]
struct TextMessage {
    #[serde(rename = "type")]
    message_type: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    substitution: Option<HashMap<String, SubstitutionValue>>,
}

impl TextMessage {
    fn text(content: impl Into<String>) -> Self {
        Self {
            message_type: "text".to_string(),
            text: content.into(),
            substitution: None,
        }
    }

    /// Build a TextMessage with @name mentions resolved against the member_cache (name -> user_id).
    /// Uses textV2 format with substitution map when mentions are found.
    fn with_mentions(content: impl Into<String>, name_to_uid: &HashMap<String, String>) -> Self {
        let mut text: String = content.into();
        let mut substitution: HashMap<String, SubstitutionValue> = HashMap::new();
        let mut counter = 0usize;

        // Collect all @name matches first to avoid modifying string while iterating
        let mut replacements: Vec<(String, String, String)> = Vec::new(); // (original "@name", placeholder, user_id)

        let chars: Vec<char> = text.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '@' {
                let start = i + 1;
                let mut end = start;
                while end < chars.len() {
                    let ch = chars[end];
                    if ch.is_alphanumeric() || ch == '_' || ch == '-' || ch > '\u{2E7F}' {
                        // Include CJK characters (> U+2E7F covers CJK unified ideographs)
                        end += 1;
                    } else {
                        break;
                    }
                }
                if end > start {
                    let name: String = chars[start..end].iter().collect();
                    if let Some(uid) = name_to_uid.get(&name) {
                        let original = format!("@{}", name);
                        let placeholder = format!("{{mention{}}}", counter);
                        replacements.push((original, placeholder, uid.clone()));
                        counter += 1;
                    }
                }
                i = end;
            } else {
                i += 1;
            }
        }

        // Apply replacements
        for (original, placeholder, uid) in &replacements {
            text = text.replacen(original, placeholder, 1);
            let key = placeholder.trim_matches(|c| c == '{' || c == '}').to_string();
            substitution.insert(key, SubstitutionValue::Mention(MentionSubstitution {
                sub_type: "mention".to_string(),
                mentionee: MentioneeRef {
                    mentionee_type: "user".to_string(),
                    user_id: Some(uid.clone()),
                },
            }));
        }

        if substitution.is_empty() {
            Self { message_type: "text".to_string(), text, substitution: None }
        } else {
            Self { message_type: "textV2".to_string(), text, substitution: Some(substitution) }
        }
    }
}

#[derive(Clone)]
struct WebhookState {
    channel_secret: String,
    channel_name: String,
    channel_access_token: String,
    allowed_users: Vec<String>,
    allowed_groups: Vec<String>,
    tx: Sender<ChannelMessage>,
    reply_tokens: Arc<Mutex<HashMap<String, (String, u64)>>>,
    member_cache: Arc<Mutex<HashMap<String, String>>>,
    bot_display_name: String,
    workspace_dir: String,
}

impl WebhookState {
    fn is_user_allowed(&self, user_id: &str) -> bool {
        self.allowed_users.iter().any(|u| u == "*" || u == user_id)
    }

    fn is_group_allowed(&self, group_id: &str) -> bool {
        self.allowed_groups.iter().any(|g| g == "*" || g == group_id)
    }
}

pub struct LineChannel {
    channel_access_token: String,
    channel_secret: String,
    webhook_port: u16,
    allowed_users: Vec<String>,
    allowed_groups: Vec<String>,
    client: Client,
    reply_tokens: Arc<Mutex<HashMap<String, (String, u64)>>>,
    member_cache: Arc<Mutex<HashMap<String, String>>>,
    bot_display_name: String,
    workspace_dir: String,
}

impl LineChannel {
    pub fn new(
        channel_access_token: impl Into<String>,
        channel_secret: impl Into<String>,
        webhook_port: u16,
        allowed_users: Vec<String>,
        allowed_groups: Vec<String>,
        bot_display_name: impl Into<String>,
        workspace_dir: impl Into<String>,
    ) -> Self {
        Self {
            channel_access_token: channel_access_token.into(),
            channel_secret: channel_secret.into(),
            webhook_port,
            allowed_users,
            allowed_groups,
            client: Client::new(),
            reply_tokens: Arc::new(Mutex::new(HashMap::new())),
            member_cache: Arc::new(Mutex::new(HashMap::new())),
            bot_display_name: bot_display_name.into(),
            workspace_dir: workspace_dir.into(),
        }
    }

    async fn pop_reply_token(&self, recipient: &str) -> Option<String> {
        let mut tokens = self.reply_tokens.lock().await;
        if let Some((token, expiry)) = tokens.remove(recipient) {
            if unix_now() < expiry {
                return Some(token);
            }
        }
        None
    }

    async fn send_reply(&self, reply_token: &str, messages: Vec<TextMessage>) -> Result<()> {
        let body = ReplyRequest {
            reply_token: reply_token.to_string(),
            messages,
        };
        let resp = self.client.post(LINE_REPLY_API)
            .bearer_auth(&self.channel_access_token)
            .json(&body)
            .send()
            .await
            .context("Failed to send reply request")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("LINE reply API error {status}: {text}"));
        }
        Ok(())
    }

    async fn send_push(&self, to: &str, messages: Vec<TextMessage>) -> Result<()> {
        let body = PushRequest {
            to: to.to_string(),
            messages,
        };
        let resp = self.client.post(LINE_PUSH_API)
            .bearer_auth(&self.channel_access_token)
            .json(&body)
            .send()
            .await
            .context("Failed to send push request")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("LINE push API error {status}: {text}"));
        }
        Ok(())
    }
}

#[async_trait]
impl Channel for LineChannel {
    fn name(&self) -> &str {
        "line"
    }

    async fn send(&self, message: &SendMessage) -> Result<()> {
        let chunks = split_message(&message.content, MAX_MESSAGE_LENGTH);
        // Build a name->user_id reverse map from the member_cache (which is user_id->name)
        let name_to_uid: std::collections::HashMap<String, String> = {
            let cache = self.member_cache.lock().await;
            let map: std::collections::HashMap<String, String> = cache.iter().map(|(uid, name)| (name.clone(), uid.clone())).collect();
            tracing::debug!("LINE mention: member_cache has {} entries, name_to_uid: {:?}", cache.len(), map.keys().collect::<Vec<_>>());
            map
        };
        let messages: Vec<TextMessage> = chunks.into_iter()
            .map(|chunk| TextMessage::with_mentions(chunk, &name_to_uid))
            .collect();

        if let Some(token) = self.pop_reply_token(&message.recipient).await {
            debug!("Using reply token for {}", message.recipient);
            match self.send_reply(&token, messages).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    warn!("Reply API failed, falling back to push: {e}");
                    let chunks = split_message(&message.content, MAX_MESSAGE_LENGTH);
                    let messages: Vec<TextMessage> = chunks.into_iter().map(TextMessage::text).collect();
                    return self.send_push(&message.recipient, messages).await;
                }
            }
        }

        debug!("Using push API for {}", message.recipient);
        self.send_push(&message.recipient, messages).await
    }

    async fn listen(&self, tx: Sender<ChannelMessage>) -> Result<()> {
        let state = WebhookState {
            channel_secret: self.channel_secret.clone(),
            channel_name: self.name().to_string(),
            channel_access_token: self.channel_access_token.clone(),
            allowed_users: self.allowed_users.clone(),
            allowed_groups: self.allowed_groups.clone(),
            tx,
            reply_tokens: Arc::clone(&self.reply_tokens),
            member_cache: Arc::clone(&self.member_cache),
            bot_display_name: self.bot_display_name.clone(),
            workspace_dir: self.workspace_dir.clone(),
        };

        let app = Router::new()
            .route("/webhook", post(handle_webhook))
            .with_state(state);

        let addr = format!("0.0.0.0:{}", self.webhook_port);
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .with_context(|| format!("Failed to bind webhook server on {addr}"))?;

        info!("LINE webhook server listening on {addr}");
        axum::serve(listener, app).await.context("LINE webhook server error")?;
        Ok(())
    }

    async fn health_check(&self) -> bool {
        match self.client.get("https://api.line.me/v2/bot/info")
            .bearer_auth(&self.channel_access_token)
            .send()
            .await
        {
            Ok(resp) => resp.status().is_success(),
            Err(e) => { warn!("LINE health check failed: {e}"); false }
        }
    }
}

/// Fetch display name for a user from LINE API.
/// For group/room messages, uses the group member profile endpoint.
/// For 1:1 messages, uses the user profile endpoint.
/// Falls back to user_id if the API call fails.
async fn fetch_display_name(
    client: &Client,
    token: &str,
    user_id: &str,
    group_id: Option<&str>,
) -> String {
    let url = if let Some(gid) = group_id {
        format!("https://api.line.me/v2/bot/group/{}/member/{}", gid, user_id)
    } else {
        format!("https://api.line.me/v2/bot/profile/{}", user_id)
    };

    let result = client
        .get(&url)
        .bearer_auth(token)
        .send()
        .await;

    match result {
        Ok(resp) if resp.status().is_success() => {
            match resp.json::<serde_json::Value>().await {
                Ok(json) => {
                    if let Some(name) = json.get("displayName").and_then(|v| v.as_str()) {
                        return name.to_string();
                    }
                    warn!("LINE profile response missing displayName for {user_id}");
                    user_id.to_string()
                }
                Err(e) => {
                    warn!("Failed to parse LINE profile response for {user_id}: {e}");
                    user_id.to_string()
                }
            }
        }
        Ok(resp) => {
            warn!("LINE profile API returned {} for {user_id}", resp.status());
            user_id.to_string()
        }
        Err(e) => {
            warn!("LINE profile API request failed for {user_id}: {e}");
            user_id.to_string()
        }
    }
}

/// Look up a user's display name from cache, fetching from LINE API if not cached.
async fn resolve_display_name(
    state: &WebhookState,
    user_id: &str,
    group_id: Option<&str>,
) -> String {
    {
        let cache = state.member_cache.lock().await;
        if let Some(name) = cache.get(user_id) {
            return name.clone();
        }
    }

    let client = Client::new();
    let name = fetch_display_name(&client, &state.channel_access_token, user_id, group_id).await;

    {
        let mut cache = state.member_cache.lock().await;
        cache.insert(user_id.to_string(), name.clone());
    }

    name
}

async fn handle_webhook(
    State(state): State<WebhookState>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let signature = match headers.get("x-line-signature").and_then(|v| v.to_str().ok()) {
        Some(s) => s.to_string(),
        None => { warn!("Missing x-line-signature"); return StatusCode::UNAUTHORIZED; }
    };

    if !verify_signature(&state.channel_secret, &body, &signature) {
        warn!("Invalid LINE webhook signature");
        return StatusCode::UNAUTHORIZED;
    }

    let payload: WebhookPayload = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => { error!("Failed to parse webhook: {e}"); return StatusCode::BAD_REQUEST; }
    };

    for event in payload.events {
        match event {
            LineEvent::Message(evt) => handle_message_event(&state, evt).await,
            LineEvent::Follow(evt) => handle_follow_event(&state, evt).await,
            LineEvent::Unfollow(evt) => {
                info!("User unfollowed: {}", evt.source.user_id.as_deref().unwrap_or("unknown"));
            }
            LineEvent::Join(evt) => handle_join_event(&state, evt).await,
            LineEvent::Unknown => { debug!("Unknown LINE event, ignoring"); }
        }
    }

    StatusCode::OK
}

async fn handle_message_event(state: &WebhookState, evt: MessageEvent) {
    let text = match &evt.message.text {
        Some(t) if evt.message.message_type == "text" => t.clone(),
        _ => { debug!("Ignoring non-text message: {}", evt.message.message_type); return; }
    };

    info!("LINE source: type={} user={:?} group={:?} room={:?}", evt.source.source_type, evt.source.user_id, evt.source.group_id, evt.source.room_id);
    let (sender_id, reply_target, thread_ts) = resolve_source(&evt.source);

    if let Some(ref gid) = evt.source.group_id {
        if !state.is_group_allowed(gid) { return; }
    } else if let Some(ref uid) = evt.source.user_id {
        if !state.is_user_allowed(uid) { return; }
    }

    // Resolve display name and prepend to message content
    let group_id_ref = evt.source.group_id.as_deref();
    let real_user_id = evt.source.user_id.as_deref().unwrap_or("unknown");
    let display_name = resolve_display_name(state, real_user_id, group_id_ref).await;
    let content = format!("[{}] {}", display_name, text);

    let is_group = evt.source.group_id.is_some() || evt.source.room_id.is_some();
    let is_mentioned = text.contains(&format!("@{}", state.bot_display_name))
        || text.contains("\u{5c0f}\u{5141}\u{5b50}");

    if is_group && !is_mentioned {
        // Group message without mention: send as __silent: to add to in-memory history
        // without triggering AI response
        debug!("Group message without mention, sending as silent: {content}");
        let _ = state.tx.send(ChannelMessage {
            id: evt.message.id.clone(),
            sender: sender_id,
            reply_target,
            content: format!("__silent:{}", content),
            channel: state.channel_name.clone(),
            timestamp: evt.timestamp / 1000,
            thread_ts,
        }).await;
        return;
    }

    // Show loading animation (1:1 chats only, LINE API doesn't support groups)
    if evt.source.group_id.is_none() && evt.source.room_id.is_none() {
        if let Some(ref uid) = evt.source.user_id {
            let client = Client::new();
            let _ = client.post("https://api.line.me/v2/bot/chat/loading/start")
                .bearer_auth(&state.channel_access_token)
                .json(&serde_json::json!({"chatId": uid, "loadingSeconds": 20}))
                .send()
                .await;
        }
    }
    store_reply_token(&state.reply_tokens, &reply_target, &evt.reply_token).await;

    let _ = state.tx.send(ChannelMessage {
        id: evt.message.id,
        sender: sender_id,
        reply_target,
        content,
        channel: state.channel_name.clone(),
        timestamp: evt.timestamp / 1000,
        thread_ts,
    }).await;
}

async fn handle_follow_event(state: &WebhookState, evt: FollowEvent) {
    let user_id = match &evt.source.user_id {
        Some(id) => id.clone(),
        None => return,
    };
    if !state.is_user_allowed(&user_id) { return; }
    info!("New LINE follower: {user_id}");
    store_reply_token(&state.reply_tokens, &user_id, &evt.reply_token).await;
    let _ = state.tx.send(ChannelMessage {
        id: format!("follow-{}", evt.timestamp),
        sender: user_id.clone(),
        reply_target: user_id,
        content: "/follow".to_string(),
        channel: state.channel_name.clone(),
        timestamp: evt.timestamp / 1000,
        thread_ts: None,
    }).await;
}

async fn handle_join_event(state: &WebhookState, evt: JoinEvent) {
    // Access control: only respond in allowed groups
    let check_gid = evt.source.group_id.as_deref().or(evt.source.room_id.as_deref());
    if let Some(gid) = check_gid {
        if !state.is_group_allowed(gid) {
            info!("Ignoring join event for non-allowed group: {gid}");
            return;
        }
    }
    let group_id = match &evt.source.group_id {
        Some(id) => id.clone(),
        None => {
            // Could be a room join; use room_id or a fallback
            evt.source.room_id.clone().unwrap_or_else(|| "unknown".into())
        }
    };

    info!("Bot joined group/room: {group_id}");
    store_reply_token(&state.reply_tokens, &group_id, &evt.reply_token).await;

    let _ = state.tx.send(ChannelMessage {
        id: format!("join-{}", evt.timestamp),
        sender: "system".to_string(),
        reply_target: group_id,
        content: "/join".to_string(),
        channel: state.channel_name.clone(),
        timestamp: evt.timestamp / 1000,
        thread_ts: None,
    }).await;
}

fn resolve_source(source: &EventSource) -> (String, String, Option<String>) {
    let user_id = source.user_id.clone().unwrap_or_else(|| "unknown".into());
    match source.source_type.as_str() {
        "group" => {
            let gid = source.group_id.clone().unwrap_or_else(|| user_id.clone());
            (gid.clone(), gid, Some(user_id))
        }

        "room" => {
            let rid = source.room_id.clone().unwrap_or_else(|| user_id.clone());
            (user_id.clone(), rid, Some(user_id))
        }
        _ => (user_id.clone(), user_id, None),
    }
}

fn verify_signature(secret: &str, body: &[u8], signature: &str) -> bool {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    BASE64.encode(mac.finalize().into_bytes()) == signature
}

async fn store_reply_token(
    tokens: &Arc<Mutex<HashMap<String, (String, u64)>>>,
    key: &str,
    token: &str,
) {
    let mut map = tokens.lock().await;
    map.retain(|_, (_, exp)| *exp > unix_now());
    map.insert(key.to_string(), (token.to_string(), unix_now() + REPLY_TOKEN_TTL_SECS));
}

fn split_message(content: &str, max_len: usize) -> Vec<String> {
    if content.len() <= max_len {
        return vec![content.to_string()];
    }
    let mut chunks = Vec::new();
    let mut remaining = content;
    while !remaining.is_empty() {
        let mut idx = max_len.min(remaining.len());
        while idx < remaining.len() && !remaining.is_char_boundary(idx) {
            idx -= 1;
        }
        chunks.push(remaining[..idx].to_string());
        remaining = &remaining[idx..];
    }
    chunks
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}
