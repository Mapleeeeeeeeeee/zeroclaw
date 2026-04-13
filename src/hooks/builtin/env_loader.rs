use std::path::Path;

/// Read a single key from the workspace `.env` file.
///
/// Matches the parsing used by `src/tools/pushover.rs`: ignores `#` comments
/// and blank lines, strips optional `export ` prefix, trims surrounding
/// single/double quotes.
pub(super) fn load_env_value(workspace_dir: &Path, key: &str) -> anyhow::Result<String> {
    let env_path = workspace_dir.join(".env");
    let content = std::fs::read_to_string(&env_path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {e}", env_path.display()))?;

    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let line = line.strip_prefix("export ").map(str::trim).unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else { continue; };
        if k.trim().eq_ignore_ascii_case(key) {
            let v = v.trim();
            let v = v.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                .unwrap_or(v);
            return Ok(v.trim().to_string());
        }
    }
    Err(anyhow::anyhow!("{key} not found in {}", env_path.display()))
}
