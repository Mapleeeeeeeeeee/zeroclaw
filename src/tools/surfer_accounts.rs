use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use regex::Regex;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

static NAME_RE: OnceLock<Regex> = OnceLock::new();

fn name_regex() -> &'static Regex {
    NAME_RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9_]{1,50}$").unwrap())
}

fn resolve_path(platform: &str, twitter: &Path, reddit: &Path) -> anyhow::Result<PathBuf> {
    match platform {
        "twitter" => Ok(twitter.to_path_buf()),
        "reddit" => Ok(reddit.to_path_buf()),
        other => anyhow::bail!("unknown platform: {other:?}; expected \"twitter\" or \"reddit\""),
    }
}

fn validate_name(name: &str) -> anyhow::Result<()> {
    if name_regex().is_match(name) {
        Ok(())
    } else {
        anyhow::bail!("invalid name {name:?}: must match ^[A-Za-z0-9_]{{1,50}}$")
    }
}

// ---------------------------------------------------------------------------
// SurferListAccountsTool
// ---------------------------------------------------------------------------

pub struct SurferListAccountsTool {
    twitter_file: PathBuf,
    reddit_file: PathBuf,
}

impl SurferListAccountsTool {
    pub fn new(twitter_file: PathBuf, reddit_file: PathBuf) -> Self {
        Self {
            twitter_file,
            reddit_file,
        }
    }
}

#[async_trait]
impl Tool for SurferListAccountsTool {
    fn name(&self) -> &str {
        "surfer_list_accounts"
    }

    fn description(&self) -> &str {
        "List all accounts/subreddits currently tracked by the Twitter/Reddit surfer. \
         MUST invoke this tool to answer any question about who/what the surfer is tracking. \
         NEVER answer from memory or prior context — the list changes and you cannot know it without calling this tool. \
         Use platform=\"twitter\" for X/Twitter handles, platform=\"reddit\" for subreddits."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "platform": {
                    "type": "string",
                    "enum": ["twitter", "reddit"],
                    "description": "Which surfer list to read."
                }
            },
            "required": ["platform"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let platform = match args["platform"].as_str() {
            Some(p) => p,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("missing required parameter: platform".to_string()),
                });
            }
        };

        let file_path = match resolve_path(platform, &self.twitter_file, &self.reddit_file) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                });
            }
        };

        let content = match tokio::fs::read_to_string(&file_path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("failed to read file: {e}")),
                });
            }
        };

        let accounts: Vec<&str> = content
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&accounts)?,
            error: None,
        })
    }
}

// ---------------------------------------------------------------------------
// SurferAddAccountTool
// ---------------------------------------------------------------------------

pub struct SurferAddAccountTool {
    twitter_file: PathBuf,
    reddit_file: PathBuf,
}

impl SurferAddAccountTool {
    pub fn new(twitter_file: PathBuf, reddit_file: PathBuf) -> Self {
        Self {
            twitter_file,
            reddit_file,
        }
    }
}

#[async_trait]
impl Tool for SurferAddAccountTool {
    fn name(&self) -> &str {
        "surfer_add_account"
    }

    fn description(&self) -> &str {
        "Add a Twitter account handle or Reddit subreddit to the surfer tracking list. \
         This is the ONLY way to actually add an entry — claiming 'added' without invoking this tool is a lie and the user will notice. \
         ALWAYS invoke this tool whenever the user asks to add, track, follow, or include any Twitter handle or subreddit. \
         Deduplicates automatically (safe to call even if entry may already exist). \
         platform=\"twitter\" for X/Twitter, platform=\"reddit\" for subreddits. \
         name must be the bare handle/subreddit name without @ or r/ prefix."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "platform": {
                    "type": "string",
                    "enum": ["twitter", "reddit"],
                    "description": "Which surfer list to modify."
                },
                "name": {
                    "type": "string",
                    "description": "Account handle or subreddit name (alphanumeric + underscore, 1-50 chars)."
                }
            },
            "required": ["platform", "name"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let platform = match args["platform"].as_str() {
            Some(p) => p,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("missing required parameter: platform".to_string()),
                });
            }
        };

        let name = match args["name"].as_str() {
            Some(n) => n,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("missing required parameter: name".to_string()),
                });
            }
        };

        if let Err(e) = validate_name(name) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(e.to_string()),
            });
        }

        let file_path = match resolve_path(platform, &self.twitter_file, &self.reddit_file) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                });
            }
        };

        let content = match tokio::fs::read_to_string(&file_path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("failed to read file: {e}")),
                });
            }
        };

        let mut lines: Vec<String> = content
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect();

        let already_present = lines.iter().any(|l| l.eq_ignore_ascii_case(name));
        let added = !already_present;

        if added {
            lines.push(name.to_owned());
            let new_content = lines.join("\n") + "\n";
            if let Some(parent) = file_path.parent() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("failed to create parent directories: {e}")),
                    });
                }
            }
            if let Err(e) = tokio::fs::write(&file_path, new_content).await {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("failed to write file: {e}")),
                });
            }
        }

        let total = lines.len();
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "platform": platform,
                "name": name,
                "added": added,
                "total": total
            }))?,
            error: None,
        })
    }
}

// ---------------------------------------------------------------------------
// SurferRemoveAccountTool
// ---------------------------------------------------------------------------

pub struct SurferRemoveAccountTool {
    twitter_file: PathBuf,
    reddit_file: PathBuf,
}

impl SurferRemoveAccountTool {
    pub fn new(twitter_file: PathBuf, reddit_file: PathBuf) -> Self {
        Self {
            twitter_file,
            reddit_file,
        }
    }
}

#[async_trait]
impl Tool for SurferRemoveAccountTool {
    fn name(&self) -> &str {
        "surfer_remove_account"
    }

    fn description(&self) -> &str {
        "Remove a Twitter account handle or Reddit subreddit from the surfer tracking list. \
         This is the ONLY way to actually remove an entry — claiming 'removed' without invoking this tool does NOT remove anything, and the user WILL verify. \
         ALWAYS invoke this tool whenever the user asks to remove, unfollow, delete, stop tracking, or drop any Twitter handle or subreddit. \
         Do not answer until the tool has returned — failure to invoke is equivalent to lying. \
         platform=\"twitter\" for X/Twitter, platform=\"reddit\" for subreddits. \
         name must be the bare handle/subreddit name without @ or r/ prefix. \
         If the name does not exist, the tool returns removed=false — not an error; report that result truthfully."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "platform": {
                    "type": "string",
                    "enum": ["twitter", "reddit"],
                    "description": "Which surfer list to modify."
                },
                "name": {
                    "type": "string",
                    "description": "Account handle or subreddit name to remove."
                }
            },
            "required": ["platform", "name"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let platform = match args["platform"].as_str() {
            Some(p) => p,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("missing required parameter: platform".to_string()),
                });
            }
        };

        let name = match args["name"].as_str() {
            Some(n) => n,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("missing required parameter: name".to_string()),
                });
            }
        };

        let file_path = match resolve_path(platform, &self.twitter_file, &self.reddit_file) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                });
            }
        };

        let content = match tokio::fs::read_to_string(&file_path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("failed to read file: {e}")),
                });
            }
        };

        let original: Vec<String> = content
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect();

        let filtered: Vec<String> = original
            .iter()
            .filter(|l| !l.eq_ignore_ascii_case(name))
            .cloned()
            .collect();

        let removed = filtered.len() < original.len();

        if removed {
            let new_content = if filtered.is_empty() {
                String::new()
            } else {
                filtered.join("\n") + "\n"
            };
            if let Err(e) = tokio::fs::write(&file_path, new_content).await {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("failed to write file: {e}")),
                });
            }
        }

        let total = filtered.len();
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "platform": platform,
                "name": name,
                "removed": removed,
                "total": total
            }))?,
            error: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn make_tools(
        tmp: &TempDir,
    ) -> (
        SurferListAccountsTool,
        SurferAddAccountTool,
        SurferRemoveAccountTool,
    ) {
        let twitter = tmp.path().join("accounts.txt");
        let reddit = tmp.path().join("reddit-subs.txt");
        (
            SurferListAccountsTool::new(twitter.clone(), reddit.clone()),
            SurferAddAccountTool::new(twitter.clone(), reddit.clone()),
            SurferRemoveAccountTool::new(twitter, reddit),
        )
    }

    async fn write_file(path: &Path, accounts: &[&str]) {
        let content = accounts.join("\n") + "\n";
        tokio::fs::write(path, content).await.unwrap();
    }

    #[tokio::test]
    async fn list_returns_accounts() {
        let tmp = TempDir::new().unwrap();
        let twitter = tmp.path().join("accounts.txt");
        write_file(&twitter, &["AccountA", "AccountB", "AccountC"]).await;

        let (list_tool, _, _) = make_tools(&tmp);
        let result = list_tool
            .execute(json!({"platform": "twitter"}))
            .await
            .unwrap();

        assert!(
            result.success,
            "expected success but got error: {:?}",
            result.error
        );
        let parsed: Vec<String> = serde_json::from_str(&result.output).unwrap();
        assert_eq!(parsed.len(), 3);
        assert!(parsed.contains(&"AccountA".to_string()));
        assert!(parsed.contains(&"AccountB".to_string()));
        assert!(parsed.contains(&"AccountC".to_string()));
    }

    #[tokio::test]
    async fn list_invalid_platform_errors() {
        let tmp = TempDir::new().unwrap();
        let (list_tool, _, _) = make_tools(&tmp);

        let result = list_tool
            .execute(json!({"platform": "facebook"}))
            .await
            .unwrap();

        assert!(!result.success);
        let error = result.error.unwrap_or_default();
        assert!(
            error.contains("unknown platform") || error.contains("facebook"),
            "unexpected error message: {error}"
        );
    }

    #[tokio::test]
    async fn add_new_appends() {
        let tmp = TempDir::new().unwrap();
        let twitter = tmp.path().join("accounts.txt");
        write_file(&twitter, &["AccountA", "AccountB", "AccountC"]).await;

        let (list_tool, add_tool, _) = make_tools(&tmp);
        let result = add_tool
            .execute(json!({"platform": "twitter", "name": "Foo"}))
            .await
            .unwrap();

        assert!(
            result.success,
            "expected success but got error: {:?}",
            result.error
        );
        let parsed: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(parsed["added"], true);
        assert_eq!(parsed["total"], 4);
        assert_eq!(parsed["name"], "Foo");

        // Verify file now has 4 lines
        let list_result = list_tool
            .execute(json!({"platform": "twitter"}))
            .await
            .unwrap();
        let accounts: Vec<String> = serde_json::from_str(&list_result.output).unwrap();
        assert_eq!(accounts.len(), 4);
    }

    #[tokio::test]
    async fn add_duplicate_noops() {
        let tmp = TempDir::new().unwrap();
        let twitter = tmp.path().join("accounts.txt");
        write_file(&twitter, &["AccountA", "AccountB", "AccountC"]).await;

        let (list_tool, add_tool, _) = make_tools(&tmp);

        // First add
        let first = add_tool
            .execute(json!({"platform": "twitter", "name": "Foo"}))
            .await
            .unwrap();
        assert!(first.success);
        let first_parsed: serde_json::Value = serde_json::from_str(&first.output).unwrap();
        assert_eq!(first_parsed["added"], true);

        // Second add (duplicate)
        let second = add_tool
            .execute(json!({"platform": "twitter", "name": "Foo"}))
            .await
            .unwrap();
        assert!(second.success);
        let second_parsed: serde_json::Value = serde_json::from_str(&second.output).unwrap();
        assert_eq!(second_parsed["added"], false);

        // File still has 4 lines
        let list_result = list_tool
            .execute(json!({"platform": "twitter"}))
            .await
            .unwrap();
        let accounts: Vec<String> = serde_json::from_str(&list_result.output).unwrap();
        assert_eq!(accounts.len(), 4);
    }

    #[tokio::test]
    async fn remove_existing_removes() {
        let tmp = TempDir::new().unwrap();
        let twitter = tmp.path().join("accounts.txt");
        write_file(&twitter, &["AccountA", "Foo", "AccountB"]).await;

        let (list_tool, _, remove_tool) = make_tools(&tmp);
        let result = remove_tool
            .execute(json!({"platform": "twitter", "name": "Foo"}))
            .await
            .unwrap();

        assert!(
            result.success,
            "expected success but got error: {:?}",
            result.error
        );
        let parsed: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(parsed["removed"], true);
        assert_eq!(parsed["total"], 2);

        // File now has 2 lines
        let list_result = list_tool
            .execute(json!({"platform": "twitter"}))
            .await
            .unwrap();
        let accounts: Vec<String> = serde_json::from_str(&list_result.output).unwrap();
        assert_eq!(accounts.len(), 2);
        assert!(!accounts.contains(&"Foo".to_string()));
    }

    #[tokio::test]
    async fn add_invalid_name_errors() {
        let tmp = TempDir::new().unwrap();
        let (_, add_tool, _) = make_tools(&tmp);

        let result = add_tool
            .execute(json!({"platform": "twitter", "name": "foo;rm -rf /"}))
            .await
            .unwrap();

        assert!(!result.success);
        let error = result.error.unwrap_or_default();
        assert!(
            error.contains("invalid name")
                || error.contains("validation")
                || error.contains("match"),
            "expected validation error, got: {error}"
        );
    }

    #[tokio::test]
    async fn remove_missing_noops() {
        let tmp = TempDir::new().unwrap();
        let twitter = tmp.path().join("accounts.txt");
        write_file(&twitter, &["AccountA", "AccountB", "AccountC"]).await;

        let (_, _, remove_tool) = make_tools(&tmp);
        let result = remove_tool
            .execute(json!({"platform": "twitter", "name": "NonExistent"}))
            .await
            .unwrap();

        assert!(
            result.success,
            "expected success but got error: {:?}",
            result.error
        );
        let parsed: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(parsed["removed"], false);
        assert_eq!(parsed["total"], 3);
    }
}
