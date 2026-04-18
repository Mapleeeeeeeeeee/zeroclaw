use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value as JsonValue};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use toml_edit::{Array, DocumentMut, Item, Table, Value};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn repo_regex() -> &'static Regex {
    static REPO: OnceLock<Regex> = OnceLock::new();
    REPO.get_or_init(|| {
        Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._\-]{0,38}/[A-Za-z0-9][A-Za-z0-9._\-]{0,99}$").unwrap()
    })
}

fn validate_repo(repo: &str) -> anyhow::Result<()> {
    if repo_regex().is_match(repo) {
        Ok(())
    } else {
        anyhow::bail!(
            "invalid repo {:?}: must be in owner/name format (e.g. 'anthropics/claude-code'). \
             No https://, no github.com/ prefix, no trailing slash. \
             Owner and name may only contain alphanumerics, dots, hyphens, and underscores.",
            repo
        )
    }
}

/// Read the config TOML from disk and return both the parsed document and the
/// current repos list. If the `[hooks.builtin.release_monitor]` section or the
/// `repos` key are absent, an empty list is returned without an error.
fn read_repos(config_path: &Path) -> anyhow::Result<(DocumentMut, Vec<String>)> {
    let raw = std::fs::read_to_string(config_path).map_err(|e| {
        anyhow::anyhow!("failed to read config file {}: {e}", config_path.display())
    })?;

    let doc: DocumentMut = raw
        .parse()
        .map_err(|e| anyhow::anyhow!("failed to parse config TOML: {e}"))?;

    let repos = extract_repos(&doc);
    Ok((doc, repos))
}

fn extract_repos(doc: &DocumentMut) -> Vec<String> {
    let Some(hooks) = doc.get("hooks").and_then(Item::as_table) else {
        return Vec::new();
    };
    let Some(builtin) = hooks.get("builtin").and_then(Item::as_table) else {
        return Vec::new();
    };
    let Some(rm) = builtin.get("release_monitor").and_then(Item::as_table) else {
        return Vec::new();
    };
    let Some(arr) = rm.get("repos").and_then(Item::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect()
}

/// Write the TOML document back to disk atomically (write to a `.tmp` file,
/// then rename), preserving all comments and formatting for sections we did
/// not touch.
fn write_document_atomic(path: &Path, doc: &DocumentMut) -> anyhow::Result<()> {
    #[cfg(unix)]
    let original_mode = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).ok().map(|m| m.permissions().mode())
    };
    let tmp_path = path.with_extension("toml.tmp");
    std::fs::write(&tmp_path, doc.to_string())
        .map_err(|e| anyhow::anyhow!("failed to write temp file {}: {e}", tmp_path.display()))?;
    #[cfg(unix)]
    if let Some(mode) = original_mode {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(mode));
    }
    std::fs::rename(&tmp_path, path).map_err(|e| {
        anyhow::anyhow!(
            "failed to rename {} → {}: {e}",
            tmp_path.display(),
            path.display()
        )
    })?;
    Ok(())
}

/// Ensure the nested table path `hooks.builtin.release_monitor` exists and
/// return a mutable reference to the `release_monitor` table's `repos` array,
/// creating all missing intermediaries as needed.
///
/// This modifies `doc` in-place and returns the new repos count after the
/// caller has made its mutation — we pass the array in rather than return a
/// reference to avoid lifetime issues, so callers mutate the doc directly
/// using `set_repos_array`.
fn ensure_repos_array(doc: &mut DocumentMut) -> Vec<String> {
    // Ensure hooks table
    if !doc.contains_key("hooks") {
        doc["hooks"] = Item::Table(Table::new());
    }
    let hooks = doc["hooks"].as_table_mut().unwrap();

    // Ensure builtin table
    if !hooks.contains_key("builtin") {
        hooks["builtin"] = Item::Table(Table::new());
    }
    let builtin = hooks["builtin"].as_table_mut().unwrap();

    // Ensure release_monitor table
    if !builtin.contains_key("release_monitor") {
        builtin["release_monitor"] = Item::Table(Table::new());
    }
    let rm = builtin["release_monitor"].as_table_mut().unwrap();

    // Ensure repos array
    if !rm.contains_key("repos") {
        rm["repos"] = Item::Value(Value::Array(Array::new()));
    }

    // Return the current list
    rm.get("repos")
        .and_then(Item::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Replace the `repos` array inside the document with the given list.
fn set_repos_array(doc: &mut DocumentMut, repos: &[String]) {
    let mut arr = Array::new();
    for r in repos {
        arr.push(r.as_str());
    }

    let hooks = doc["hooks"].as_table_mut().unwrap();
    let builtin = hooks["builtin"].as_table_mut().unwrap();
    let rm = builtin["release_monitor"].as_table_mut().unwrap();
    rm["repos"] = Item::Value(Value::Array(arr));
}

/// Spawn a detached daemon restart. Returns `true` if the spawn call itself
/// succeeded (best-effort — the sudo command may fail later, which is fine).
fn spawn_restart() -> bool {
    match tokio::process::Command::new("sh")
        .arg("-c")
        .arg("(sleep 2 && sudo -n /bin/systemctl restart zeroclaw) >/dev/null 2>&1 &")
        .spawn()
    {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("release_monitor_repos: failed to spawn restart command: {e}");
            false
        }
    }
}

// ---------------------------------------------------------------------------
// ReleaseMonitorListReposTool
// ---------------------------------------------------------------------------

pub struct ReleaseMonitorListReposTool {
    config_path: PathBuf,
}

impl ReleaseMonitorListReposTool {
    pub fn new(config_path: PathBuf) -> Self {
        Self { config_path }
    }
}

#[async_trait]
impl Tool for ReleaseMonitorListReposTool {
    fn name(&self) -> &str {
        "release_monitor_list_repos"
    }

    fn description(&self) -> &str {
        "List all GitHub repositories currently tracked by the release monitor. \
         When a new release is published for a tracked repo, the daemon sends a Telegram notification. \
         \n\
         MUST invoke this tool to answer ANY question about which repos are tracked. \
         NEVER answer from memory or prior context — the list is stored on disk and changes \
         independently of this conversation; you cannot know its current state without calling \
         this tool. \
         Do not answer until the tool has returned."
    }

    fn parameters_schema(&self) -> JsonValue {
        json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        })
    }

    async fn execute(&self, _args: JsonValue) -> anyhow::Result<ToolResult> {
        // If the config file doesn't exist yet, treat it as empty list.
        if !self.config_path.exists() {
            return Ok(ToolResult {
                success: true,
                output: serde_json::to_string_pretty(&json!({ "repos": [], "count": 0 }))?,
                error: None,
            });
        }

        let repos = match read_repos(&self.config_path) {
            Ok((_, repos)) => repos,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                });
            }
        };

        let count = repos.len();
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({ "repos": repos, "count": count }))?,
            error: None,
        })
    }
}

// ---------------------------------------------------------------------------
// ReleaseMonitorAddRepoTool
// ---------------------------------------------------------------------------

pub struct ReleaseMonitorAddRepoTool {
    config_path: PathBuf,
}

impl ReleaseMonitorAddRepoTool {
    pub fn new(config_path: PathBuf) -> Self {
        Self { config_path }
    }
}

#[async_trait]
impl Tool for ReleaseMonitorAddRepoTool {
    fn name(&self) -> &str {
        "release_monitor_add_repo"
    }

    fn description(&self) -> &str {
        "Add a GitHub repository to the release monitor tracking list. \
         When a new release is published for a tracked repo, the daemon sends a Telegram notification. \
         \n\
         This is the ONLY way to actually add a repo — claiming 'added' without invoking this tool \
         is a lie and the user will verify. \
         ALWAYS invoke this tool whenever the user asks to add, track, watch, or monitor any GitHub repo. \
         \n\
         The tool deduplicates automatically (safe to call even if the repo may already be tracked). \
         The repo argument must be in bare 'owner/name' format — no https://, no github.com/ prefix, \
         no trailing slash (e.g. 'anthropics/claude-code'). \
         \n\
         After a successful add the daemon is restarted automatically to apply the change. \
         Do not answer until the tool has returned."
    }

    fn parameters_schema(&self) -> JsonValue {
        json!({
            "type": "object",
            "properties": {
                "repo": {
                    "type": "string",
                    "description": "GitHub repo in owner/name format, e.g. 'anthropics/claude-code'. \
                                    No https://, no github.com/ prefix, no trailing slash."
                }
            },
            "required": ["repo"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: JsonValue) -> anyhow::Result<ToolResult> {
        let repo = match args["repo"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("missing required parameter: repo".to_string()),
                });
            }
        };

        // Validate format first — never panic, always return structured error.
        if let Err(e) = validate_repo(repo) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(e.to_string()),
            });
        }

        // Read existing document (or start from empty if file doesn't exist).
        let mut doc: DocumentMut = if self.config_path.exists() {
            match read_repos(&self.config_path) {
                Ok((doc, _)) => doc,
                Err(e) => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(e.to_string()),
                    });
                }
            }
        } else {
            DocumentMut::new()
        };

        // Ensure the full table chain exists and get current list.
        let mut repos = ensure_repos_array(&mut doc);

        // Case-insensitive dedup check.
        let already_present = repos.iter().any(|r| r.eq_ignore_ascii_case(repo));
        if already_present {
            let total = repos.len();
            return Ok(ToolResult {
                success: true,
                output: serde_json::to_string_pretty(&json!({
                    "repo": repo,
                    "added": false,
                    "total": total,
                    "restarted": false
                }))?,
                error: None,
            });
        }

        // Append, persist.
        repos.push(repo.to_owned());
        set_repos_array(&mut doc, &repos);

        if let Err(e) = write_document_atomic(&self.config_path, &doc) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(e.to_string()),
            });
        }

        let total = repos.len();
        let restarted = spawn_restart();
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "repo": repo,
                "added": true,
                "total": total,
                "restarted": restarted
            }))?,
            error: None,
        })
    }
}

// ---------------------------------------------------------------------------
// ReleaseMonitorRemoveRepoTool
// ---------------------------------------------------------------------------

pub struct ReleaseMonitorRemoveRepoTool {
    config_path: PathBuf,
}

impl ReleaseMonitorRemoveRepoTool {
    pub fn new(config_path: PathBuf) -> Self {
        Self { config_path }
    }
}

#[async_trait]
impl Tool for ReleaseMonitorRemoveRepoTool {
    fn name(&self) -> &str {
        "release_monitor_remove_repo"
    }

    fn description(&self) -> &str {
        "Remove a GitHub repository from the release monitor tracking list. \
         \n\
         This is the ONLY way to actually remove a repo — claiming 'removed' without invoking this \
         tool does NOT remove anything, and the user WILL verify. \
         ALWAYS invoke this tool whenever the user asks to remove, stop monitoring, unwatch, or \
         drop any GitHub repo from the release monitor. \
         \n\
         If the repo is not currently tracked the tool returns removed=false — not an error; \
         report that result truthfully. \
         The repo argument must be in bare 'owner/name' format — no https://, no github.com/ prefix, \
         no trailing slash (e.g. 'anthropics/claude-code'). \
         \n\
         After a successful removal the daemon is restarted automatically to apply the change. \
         Do not answer until the tool has returned."
    }

    fn parameters_schema(&self) -> JsonValue {
        json!({
            "type": "object",
            "properties": {
                "repo": {
                    "type": "string",
                    "description": "GitHub repo in owner/name format to remove, e.g. 'anthropics/claude-code'."
                }
            },
            "required": ["repo"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: JsonValue) -> anyhow::Result<ToolResult> {
        let repo = match args["repo"].as_str() {
            Some(r) => r,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("missing required parameter: repo".to_string()),
                });
            }
        };

        // Config doesn't exist → nothing to remove, not an error.
        if !self.config_path.exists() {
            return Ok(ToolResult {
                success: true,
                output: serde_json::to_string_pretty(&json!({
                    "repo": repo,
                    "removed": false,
                    "total": 0,
                    "restarted": false
                }))?,
                error: None,
            });
        }

        let mut doc = match read_repos(&self.config_path) {
            Ok((doc, _)) => doc,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                });
            }
        };

        // Ensure path exists and grab current list.
        let repos = ensure_repos_array(&mut doc);

        let filtered: Vec<String> = repos
            .iter()
            .filter(|r| !r.eq_ignore_ascii_case(repo))
            .cloned()
            .collect();

        let removed = filtered.len() < repos.len();

        if !removed {
            let total = repos.len();
            return Ok(ToolResult {
                success: true,
                output: serde_json::to_string_pretty(&json!({
                    "repo": repo,
                    "removed": false,
                    "total": total,
                    "restarted": false
                }))?,
                error: None,
            });
        }

        set_repos_array(&mut doc, &filtered);

        if let Err(e) = write_document_atomic(&self.config_path, &doc) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(e.to_string()),
            });
        }

        let total = filtered.len();
        let restarted = spawn_restart();
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "repo": repo,
                "removed": true,
                "total": total,
                "restarted": restarted
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
    use std::fs;
    use tempfile::TempDir;

    fn config_path(tmp: &TempDir) -> PathBuf {
        tmp.path().join("config.toml")
    }

    fn make_tools(
        tmp: &TempDir,
    ) -> (
        ReleaseMonitorListReposTool,
        ReleaseMonitorAddRepoTool,
        ReleaseMonitorRemoveRepoTool,
    ) {
        let path = config_path(tmp);
        (
            ReleaseMonitorListReposTool::new(path.clone()),
            ReleaseMonitorAddRepoTool::new(path.clone()),
            ReleaseMonitorRemoveRepoTool::new(path),
        )
    }

    fn write_config(tmp: &TempDir, content: &str) {
        fs::write(config_path(tmp), content).unwrap();
    }

    // -----------------------------------------------------------------------
    // 1. list_returns_repos
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn list_returns_repos() {
        let tmp = TempDir::new().unwrap();
        write_config(
            &tmp,
            r#"
[hooks.builtin.release_monitor]
repos = ["a/b", "c/d"]
"#,
        );

        let (list_tool, _, _) = make_tools(&tmp);
        let result = list_tool.execute(json!({})).await.unwrap();

        assert!(
            result.success,
            "expected success but got error: {:?}",
            result.error
        );
        let parsed: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(parsed["count"], 2);
        let repos = parsed["repos"].as_array().unwrap();
        assert!(repos.iter().any(|v| v.as_str() == Some("a/b")));
        assert!(repos.iter().any(|v| v.as_str() == Some("c/d")));
    }

    // -----------------------------------------------------------------------
    // 2. list_missing_section_returns_empty
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn list_missing_section_returns_empty() {
        let tmp = TempDir::new().unwrap();
        write_config(
            &tmp,
            r#"
[channels]
foo = "bar"
"#,
        );

        let (list_tool, _, _) = make_tools(&tmp);
        let result = list_tool.execute(json!({})).await.unwrap();

        assert!(
            result.success,
            "expected success but got error: {:?}",
            result.error
        );
        let parsed: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(parsed["count"], 0);
        assert_eq!(parsed["repos"].as_array().unwrap().len(), 0);
    }

    // -----------------------------------------------------------------------
    // 3. add_new_appends_and_preserves_other_sections
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn add_new_appends_and_preserves_other_sections() {
        let tmp = TempDir::new().unwrap();
        write_config(
            &tmp,
            r#"
[channels]
foo = "bar"

[hooks.builtin.release_monitor]
repos = ["a/b"]
"#,
        );

        let (list_tool, add_tool, _) = make_tools(&tmp);
        let result = add_tool
            .execute(json!({ "repo": "owner/myrepo" }))
            .await
            .unwrap();

        assert!(
            result.success,
            "expected success but got error: {:?}",
            result.error
        );
        let parsed: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(parsed["added"], true);
        assert_eq!(parsed["total"], 2);

        // Verify file on disk: repos has new entry
        let list_result = list_tool.execute(json!({})).await.unwrap();
        let list_parsed: serde_json::Value = serde_json::from_str(&list_result.output).unwrap();
        let repos = list_parsed["repos"].as_array().unwrap();
        assert_eq!(repos.len(), 2);
        assert!(repos.iter().any(|v| v.as_str() == Some("owner/myrepo")));

        // [channels] section must survive
        let raw = fs::read_to_string(config_path(&tmp)).unwrap();
        assert!(
            raw.contains("[channels]"),
            "channels section was lost: {raw}"
        );
        assert!(
            raw.contains(r#"foo = "bar""#),
            "channels.foo was lost: {raw}"
        );
    }

    // -----------------------------------------------------------------------
    // 4. add_duplicate_is_noop
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn add_duplicate_is_noop() {
        let tmp = TempDir::new().unwrap();
        write_config(
            &tmp,
            r#"
[hooks.builtin.release_monitor]
repos = ["anthropics/claude-code"]
"#,
        );

        let original_bytes = fs::read(config_path(&tmp)).unwrap();

        let (_, add_tool, _) = make_tools(&tmp);

        // Exact duplicate
        let result1 = add_tool
            .execute(json!({ "repo": "anthropics/claude-code" }))
            .await
            .unwrap();
        assert!(result1.success);
        let p1: serde_json::Value = serde_json::from_str(&result1.output).unwrap();
        assert_eq!(p1["added"], false);

        // Case-variant duplicate
        let result2 = add_tool
            .execute(json!({ "repo": "Anthropics/Claude-Code" }))
            .await
            .unwrap();
        assert!(result2.success);
        let p2: serde_json::Value = serde_json::from_str(&result2.output).unwrap();
        assert_eq!(p2["added"], false);

        // File must be unchanged
        let after_bytes = fs::read(config_path(&tmp)).unwrap();
        assert_eq!(
            original_bytes, after_bytes,
            "file was modified even though add was a no-op"
        );
    }

    // -----------------------------------------------------------------------
    // 5. add_invalid_format_errors
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn add_invalid_format_errors() {
        let tmp = TempDir::new().unwrap();
        write_config(&tmp, "[hooks.builtin.release_monitor]\nrepos = []\n");

        let (_, add_tool, _) = make_tools(&tmp);

        let invalid_inputs = [
            "not-a-repo",
            "foo",
            "owner/",
            "/name",
            "https://github.com/a/b",
        ];

        for bad in &invalid_inputs {
            let result = add_tool.execute(json!({ "repo": bad })).await.unwrap();
            assert!(
                !result.success,
                "expected failure for {:?} but got success",
                bad
            );
            assert!(
                result.error.is_some(),
                "expected error message for {:?}",
                bad
            );
        }
    }

    // -----------------------------------------------------------------------
    // 6. remove_existing
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn remove_existing() {
        let tmp = TempDir::new().unwrap();
        write_config(
            &tmp,
            r#"
[hooks.builtin.release_monitor]
repos = ["first/repo", "middle/repo", "last/repo"]
"#,
        );

        let (list_tool, _, remove_tool) = make_tools(&tmp);
        let result = remove_tool
            .execute(json!({ "repo": "middle/repo" }))
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

        let list_result = list_tool.execute(json!({})).await.unwrap();
        let list_parsed: serde_json::Value = serde_json::from_str(&list_result.output).unwrap();
        let repos = list_parsed["repos"].as_array().unwrap();
        assert_eq!(repos.len(), 2);
        assert!(
            repos.iter().all(|v| v.as_str() != Some("middle/repo")),
            "middle/repo was not removed"
        );
        assert!(repos.iter().any(|v| v.as_str() == Some("first/repo")));
        assert!(repos.iter().any(|v| v.as_str() == Some("last/repo")));
    }

    // -----------------------------------------------------------------------
    // 7. remove_missing_is_noop
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn remove_missing_is_noop() {
        let tmp = TempDir::new().unwrap();
        write_config(
            &tmp,
            r#"
[hooks.builtin.release_monitor]
repos = ["a/b"]
"#,
        );

        let original_bytes = fs::read(config_path(&tmp)).unwrap();

        let (_, _, remove_tool) = make_tools(&tmp);
        let result = remove_tool.execute(json!({ "repo": "c/d" })).await.unwrap();

        assert!(
            result.success,
            "expected success but got error: {:?}",
            result.error
        );
        let parsed: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(parsed["removed"], false);
        assert_eq!(parsed["total"], 1);

        let after_bytes = fs::read(config_path(&tmp)).unwrap();
        assert_eq!(
            original_bytes, after_bytes,
            "file was modified even though remove was a no-op"
        );
    }

    // -----------------------------------------------------------------------
    // 8. add_creates_section_if_missing
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn add_creates_section_if_missing() {
        let tmp = TempDir::new().unwrap();
        write_config(
            &tmp,
            r#"
[channels]
foo = "bar"
"#,
        );

        let (list_tool, add_tool, _) = make_tools(&tmp);
        let result = add_tool
            .execute(json!({ "repo": "my-org/my-project" }))
            .await
            .unwrap();

        assert!(
            result.success,
            "expected success but got error: {:?}",
            result.error
        );
        let parsed: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(parsed["added"], true);
        assert_eq!(parsed["total"], 1);

        // The section must now exist with exactly that one repo.
        let list_result = list_tool.execute(json!({})).await.unwrap();
        let list_parsed: serde_json::Value = serde_json::from_str(&list_result.output).unwrap();
        assert_eq!(list_parsed["count"], 1);
        let repos = list_parsed["repos"].as_array().unwrap();
        assert_eq!(repos[0].as_str(), Some("my-org/my-project"));

        // [channels] section must still be intact.
        let raw = fs::read_to_string(config_path(&tmp)).unwrap();
        assert!(
            raw.contains("[channels]"),
            "channels section was lost: {raw}"
        );
    }
}
